//! The control plane: what a controller may change on a running capsule, served at `/control` on
//! the A2A door's listener.
//!
//! **A separate plane, not a door method.** A control method is not an A2A method, and the door is
//! a conformant A2A surface: a murmur-only JSON-RPC method on `POST /` would make it a dialect. The
//! door's callers and a capsule's controllers are also different principals. A peer allowed to
//! send a capsule a task is not thereby allowed to reconfigure it or hand it a secret, so this
//! plane has its own credential, and neither the door nor this plane accepts the other's. It shares
//! the listener so a controller reaches the capsule at the address it already resolves from
//! `~/.murmur/running/`, under the bind policy the door already has.
//!
//! **Who may call it.** Only the holder of the session's control token: a [`crate::mac_token`]
//! token of the family [`TOKEN_TAG`], sealed with a [`MintKey`] generated in memory at launch and
//! never written, and bound to the session id. A token cannot outlive its process or verify
//! against another session. The token is written owner-only to
//! [`crate::running::control_token_path`] and to nowhere else: no guest, hook, tool or shell
//! subprocess is handed its path, its contents or a variable carrying it, so the agent is never a
//! caller. A capsule that declares no `control:` block mints no token and answers `404` to every
//! request under [`CONTROL_PATH`].
//!
//! **Secrets are held in memory and recorded nowhere.** A secret is accepted only from a loopback
//! peer, since the door speaks plain HTTP. Its body is capped before it is allocated, never read
//! when authentication fails, read raw rather than through a parser whose errors could quote it,
//! and checked as a header-safe value by a refusal worded without it. It lives in a
//! [`SecretValue`], whose `Debug` redacts and whose bytes are overwritten on drop. The trace names
//! the secret and never carries its value, a hash of it, its length or any prefix — the terms
//! [`crate::gateway_credential`] states for the credentials file. Nothing a controller sets,
//! setting or secret, is persisted: a restart starts from the manifest.
//!
//! The order of checks is fixed, and it is also what leaks nothing: declared → authenticated →
//! path, name and method → loopback (secrets) → body length → content type and value.

use std::{
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock},
};

use murmur_artifact::{ControlConfig, ControllableSetting, InferenceConfig};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{
    mac_token::{self, MintKey},
    resource_plane::ResourceResponse,
    trace::{ControlChange, ResourceTraceAppender},
};

/// The prefix every control-plane path starts with. `/control` itself is the listing.
pub(crate) const CONTROL_PATH: &str = "/control";

/// The version tag of a control token.
pub(crate) const TOKEN_TAG: &str = "ctl1";

/// The MAC domain of a control token. Distinct from every other family's, so no other token this
/// process mints can verify here.
pub(crate) const TOKEN_DOMAIN: &[u8] = b"murmur-control-token-v1";

/// The largest settings body read, in bytes. A longer declared length is refused unread.
pub(crate) const SETTINGS_BODY_MAX_BYTES: usize = 1024;

/// The largest secret body read, in bytes. A longer declared length is refused unread.
pub(crate) const SECRET_BODY_MAX_BYTES: usize = 8192;

/// When an accepted setting change takes effect, as `applies_from` states it.
pub(crate) const APPLIES_FROM: &str = "next_inference_call";

/// The challenge every `401` carries.
const BEARER_CHALLENGE: &str = "Bearer realm=\"murmur-control\"";

const SETTINGS_PREFIX: &str = "/control/settings/";
const SECRETS_PREFIX: &str = "/control/secrets/";

// ── Secret values ─────────────────────────────────────────────────────────────

/// One injected secret's bytes, exactly as the controller sent them.
///
/// Derives nothing and implements only `Debug`, which prints `<redacted>`, and `Drop`, which
/// overwrites the bytes. There is no `Clone`, `Display` or `Serialize`, so the value cannot be
/// copied into a log line, a trace event or an error by any route but [`Self::expose`].
pub(crate) struct SecretValue(Vec<u8>);

impl SecretValue {
    /// Takes ownership of `bytes`. The caller allocated them at their exact length, so the vector
    /// holds no spare capacity that a later drop would leave unwritten.
    fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Whether the value can travel in an HTTP header: non-empty, and every byte visible ASCII or
    /// a space. Refusals from here name no byte.
    fn is_header_safe(&self) -> bool {
        !self.0.is_empty() && self.0.iter().all(|byte| (0x20..=0x7e).contains(byte))
    }

    /// The value as text, for the one caller that must render it into a request header. Valid
    /// only for a value [`Self::is_header_safe`] accepted, which every stored value is.
    fn expose(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// Overwrites every byte with volatile writes, as [`MintKey`] does its key.
    fn zeroize(&mut self) {
        mac_token::zeroize(&mut self.0);
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The secrets `control.secrets` declares, each held or not, for one session.
///
/// Shared between the control plane, which sets and forgets values, and each injected
/// [`crate::gateway_credential::GatewayCredential`], which reads one on every keyed request.
pub(crate) struct InjectedSecrets {
    /// One slot per declared name, in manifest order.
    slots: Mutex<Vec<(String, Option<SecretValue>)>>,
}

impl InjectedSecrets {
    pub(crate) fn new(names: &[String]) -> Self {
        Self {
            slots: Mutex::new(names.iter().map(|name| (name.clone(), None)).collect()),
        }
    }

    fn with_slot<T>(&self, name: &str, f: impl FnOnce(&mut Option<SecretValue>) -> T) -> Option<T> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter_mut()
            .find(|(declared, _)| declared == name)
            .map(|(_, slot)| f(slot))
    }

    /// Whether `name` is declared.
    pub(crate) fn declares(&self, name: &str) -> bool {
        self.with_slot(name, |_| ()).is_some()
    }

    /// A copy of `name`'s value for one request, or `None` when nothing is held for it.
    ///
    /// The copy is the rendered header's source and is dropped with the request; the held value
    /// stays in its [`SecretValue`].
    pub(crate) fn value(&self, name: &str) -> Option<String> {
        self.with_slot(name, |slot| {
            slot.as_ref().map(|value| value.expose().to_string())
        })
        .flatten()
    }

    /// Stores `value` for `name`. `Some(replaced)` when `name` is declared, `None` when it is not.
    fn set(&self, name: &str, value: SecretValue) -> Option<bool> {
        self.with_slot(name, |slot| slot.replace(value).is_some())
    }

    /// Drops `name`'s value. `Some(was_set)` when `name` is declared.
    fn forget(&self, name: &str) -> Option<bool> {
        self.with_slot(name, |slot| slot.take().is_some())
    }

    /// Every declared name and whether a value is held for it, in manifest order.
    fn listing(&self) -> Vec<(String, bool)> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter()
            .map(|(name, slot)| (name.clone(), slot.is_some()))
            .collect()
    }
}

impl std::fmt::Debug for InjectedSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut map = f.debug_map();
        for (name, set) in self.listing() {
            map.entry(&name, &if set { "<redacted>" } else { "<unset>" });
        }
        map.finish()
    }
}

// ── Settings ──────────────────────────────────────────────────────────────────

/// A setting's value as the plane validated it. One variant per value shape a
/// [`ControllableSetting`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingValue {
    /// A token count, `1..=u32::MAX`.
    Tokens(u32),
}

impl SettingValue {
    fn to_json(self) -> Value {
        match self {
            Self::Tokens(count) => json!(count),
        }
    }

    /// The token count, for a setting whose value is one.
    pub(crate) fn tokens(self) -> u32 {
        match self {
            Self::Tokens(count) => count,
        }
    }
}

/// The value `setting` starts the session with: what the manifest resolved to.
fn launch_value(setting: ControllableSetting, inference: &InferenceConfig) -> SettingValue {
    match setting {
        ControllableSetting::InferenceMaxTokens => SettingValue::Tokens(
            inference
                .max_tokens
                .unwrap_or(crate::agent::DEFAULT_MAX_OUTPUT_TOKENS),
        ),
    }
}

/// `value` as `setting` takes it, or the one-sentence reason it does not.
fn parse_setting_value(
    setting: ControllableSetting,
    value: &Value,
) -> Result<SettingValue, String> {
    match setting {
        ControllableSetting::InferenceMaxTokens => value
            .as_u64()
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count >= 1)
            .map(SettingValue::Tokens)
            .ok_or_else(|| {
                format!(
                    "{} takes an integer from 1 to {}",
                    setting.wire_name(),
                    u32::MAX
                )
            }),
    }
}

struct SettingSlot {
    setting: ControllableSetting,
    value: SettingValue,
    /// The `event_id` of the `control_change` that set `value`, `None` for the launch value.
    change_id: Option<String>,
    /// The value the last inference call read, so the first call to read a different one can
    /// say so with `control_applied`.
    last_used: SettingValue,
}

/// What a controller has changed on this session, read by the agent loop and the gateways.
pub(crate) struct ControlState {
    settings: Mutex<Vec<SettingSlot>>,
    secrets: Arc<InjectedSecrets>,
}

impl std::fmt::Debug for ControlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlState")
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

/// One inference call's reading of a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettingForCall {
    pub(crate) value: SettingValue,
    /// The `control_change` this call is the first to apply, `None` when the previous call read
    /// the same value.
    pub(crate) applies_change: Option<String>,
}

impl ControlState {
    /// State for a session that declares `config`, every setting at the value `inference`
    /// resolves it to and no secret held.
    pub(crate) fn new(config: &ControlConfig, inference: &InferenceConfig) -> Self {
        let settings = config
            .settings
            .iter()
            .map(|setting| {
                let value = launch_value(*setting, inference);
                SettingSlot {
                    setting: *setting,
                    value,
                    change_id: None,
                    last_used: value,
                }
            })
            .collect();
        Self {
            settings: Mutex::new(settings),
            secrets: Arc::new(InjectedSecrets::new(&config.secrets)),
        }
    }

    /// The secrets store every injected gateway credential reads.
    pub(crate) fn secrets(&self) -> Arc<InjectedSecrets> {
        Arc::clone(&self.secrets)
    }

    fn lock_settings(&self) -> std::sync::MutexGuard<'_, Vec<SettingSlot>> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn setting_value(&self, setting: ControllableSetting) -> Option<SettingValue> {
        self.lock_settings()
            .iter()
            .find(|slot| slot.setting == setting)
            .map(|slot| slot.value)
    }

    /// Sets `setting` to `value` under `change_id` and returns the value it replaces, or `None`
    /// for a setting this session does not declare.
    fn change_setting(
        &self,
        setting: ControllableSetting,
        value: SettingValue,
        change_id: String,
    ) -> Option<SettingValue> {
        let mut settings = self.lock_settings();
        let slot = settings.iter_mut().find(|slot| slot.setting == setting)?;
        let previous = slot.value;
        slot.value = value;
        slot.change_id = Some(change_id);
        Some(previous)
    }

    /// The value one inference call uses for `setting`, read once so the call never mixes two.
    /// `None` for a setting this session does not declare.
    pub(crate) fn for_call(&self, setting: ControllableSetting) -> Option<SettingForCall> {
        let mut settings = self.lock_settings();
        let slot = settings.iter_mut().find(|slot| slot.setting == setting)?;
        let applies_change = (slot.value != slot.last_used)
            .then(|| slot.change_id.clone())
            .flatten();
        slot.last_used = slot.value;
        Some(SettingForCall {
            value: slot.value,
            applies_change,
        })
    }

    /// What `session_start.control` records: every declared name, never a value.
    pub(crate) fn session_control(&self) -> crate::trace::SessionControl {
        crate::trace::SessionControl {
            settings: self
                .lock_settings()
                .iter()
                .map(|slot| slot.setting.wire_name())
                .collect(),
            secrets: self
                .secrets
                .listing()
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
        }
    }

    fn settings_listing(&self) -> Vec<(ControllableSetting, SettingValue)> {
        self.lock_settings()
            .iter()
            .map(|slot| (slot.setting, slot.value))
            .collect()
    }
}

// ── Token ─────────────────────────────────────────────────────────────────────

/// Mints `session_id`'s control token under `key`.
pub(crate) fn mint_control_token(key: &MintKey, session_id: &str) -> Result<String, String> {
    let payload = json!({
        "session_id": session_id,
        "nonce": mac_token::random_hex(mac_token::NONCE_BYTES)?,
    });
    Ok(mac_token::seal(
        key,
        TOKEN_TAG,
        TOKEN_DOMAIN,
        payload.to_string().as_bytes(),
        &[session_id],
    ))
}

/// Whether `token` is a control token `key` minted for `session_id`. Compares in constant time,
/// and checks the payload only once the MAC has passed.
pub(crate) fn verify_control_token(key: &MintKey, session_id: &str, token: &str) -> bool {
    let Ok(payload) = mac_token::open(key, TOKEN_TAG, TOKEN_DOMAIN, token, &[session_id]) else {
        return false;
    };
    serde_json::from_slice::<Value>(&payload)
        .ok()
        .and_then(|payload| payload["session_id"].as_str().map(|id| id == session_id))
        .unwrap_or(false)
}

/// Whether `peer` is this host: IPv4 `127.0.0.0/8`, IPv6 `::1`, or an IPv4-mapped IPv6 address
/// in `127.0.0.0/8`.
pub(crate) fn is_loopback_peer(peer: IpAddr) -> bool {
    match peer {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

// ── The plane ─────────────────────────────────────────────────────────────────

struct Declared {
    key: MintKey,
    state: Arc<ControlState>,
}

/// One session's control plane: its key and state when `control:` is declared, and the trace it
/// records to.
pub(crate) struct ControlPlane {
    declared: Option<Declared>,
    session_id: String,
    /// Set once the session trace is open. Events are written only when it is set.
    trace: OnceLock<Arc<ResourceTraceAppender>>,
}

impl std::fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlane")
            .field("session_id", &self.session_id)
            .field("declared", &self.declared.is_some())
            .finish_non_exhaustive()
    }
}

impl ControlPlane {
    /// The plane of a capsule with no `control:` block: every request is `404`.
    pub(crate) fn undeclared(session_id: String) -> Self {
        Self {
            declared: None,
            session_id,
            trace: OnceLock::new(),
        }
    }

    /// The plane of a capsule that declares `state`, with a freshly generated key, and the token
    /// it minted for this session. The key never leaves the returned plane.
    pub(crate) fn declared(
        session_id: String,
        state: Arc<ControlState>,
    ) -> Result<(Self, String), String> {
        let key = MintKey::generate()?;
        let token = mint_control_token(&key, &session_id)?;
        Ok((
            Self {
                declared: Some(Declared { key, state }),
                session_id,
                trace: OnceLock::new(),
            },
            token,
        ))
    }

    /// Hands the plane the session trace's appender. A second call is ignored.
    pub(crate) fn attach_trace(&self, trace: Arc<ResourceTraceAppender>) {
        let _ = self.trace.set(trace);
    }

    async fn record_refused(
        &self,
        refusal: &Refusal<'_>,
        authenticated: Option<&Authenticated<'_>>,
    ) {
        let Some(trace) = self.trace.get() else {
            return;
        };
        let (kind, name, token_id) = match authenticated {
            Some(auth) => (
                refusal.target.map(|target| target.kind),
                refusal.target.map(|target| target.name),
                Some(auth.token_id.as_str()),
            ),
            None => (None, None, None),
        };
        trace
            .write_control_refused(refusal.status, refusal.reason, kind, name, token_id)
            .await;
    }

    async fn record_change(&self, change: ControlChange<'_>) {
        if let Some(trace) = self.trace.get() {
            trace.write_control_change(change).await;
        }
    }
}

/// Is `path` under the control plane's prefix.
pub(crate) fn is_control_path(path: &str) -> bool {
    path == CONTROL_PATH || path.starts_with("/control/") || path.starts_with("/control?")
}

/// One request as the door read it, before any body.
pub(crate) struct ControlRequest<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    /// The `authorization` header value as sent, case preserved.
    pub(crate) authorization: Option<&'a str>,
    /// The connection's peer address; `None` when the door could not read it.
    pub(crate) peer: Option<IpAddr>,
    pub(crate) content_length: usize,
    /// Whether a `content-type` header named `application/json`.
    pub(crate) is_json: bool,
}

/// What a request names once authentication has passed: the kind of resource and the name in
/// its path, declared or not.
#[derive(Clone, Copy)]
struct Target<'a> {
    kind: &'static str,
    name: &'a str,
}

impl<'a> Target<'a> {
    fn setting(name: &'a str) -> Self {
        Self {
            kind: "setting",
            name,
        }
    }

    fn secret(name: &'a str) -> Self {
        Self {
            kind: "secret",
            name,
        }
    }
}

struct Authenticated<'a> {
    token_id: String,
    state: &'a ControlState,
}

/// A refused request: its status, the trace's reason word, the one sentence the body carries,
/// and what it named once authentication had passed.
struct Refusal<'a> {
    status: u16,
    reason: &'static str,
    message: String,
    target: Option<Target<'a>>,
    allow: Option<&'static str>,
}

impl<'a> Refusal<'a> {
    fn new(status: u16, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            message: message.into(),
            target: None,
            allow: None,
        }
    }

    fn naming(mut self, target: Target<'a>) -> Self {
        self.target = Some(target);
        self
    }

    fn response(&self) -> ResourceResponse {
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        if self.status == 401 {
            headers.push(("www-authenticate".to_string(), BEARER_CHALLENGE.to_string()));
        }
        if let Some(allow) = self.allow {
            headers.push(("allow".to_string(), allow.to_string()));
        }
        json_response(self.status, headers, &json!({"error": self.message}))
    }
}

fn json_response(status: u16, headers: Vec<(String, String)>, body: &Value) -> ResourceResponse {
    ResourceResponse::framed(status, headers, body.to_string().into_bytes())
}

fn ok(body: Value) -> ResourceResponse {
    json_response(
        200,
        vec![("content-type".to_string(), "application/json".to_string())],
        &body,
    )
}

/// Serves one request under [`CONTROL_PATH`], reading its body from `body` only once every check
/// that can be made without it has passed.
///
/// Answers every method under the prefix, refusals included, and records every refusal of a
/// declared plane as `control_refused` and every accepted change as `control_change`.
pub(crate) async fn handle_control_request<R: AsyncRead + Unpin>(
    plane: &ControlPlane,
    request: ControlRequest<'_>,
    body: &mut R,
) -> ResourceResponse {
    let Some(declared) = plane.declared.as_ref() else {
        return Refusal::new(
            404,
            "undeclared",
            "this capsule declares no control surface",
        )
        .response();
    };

    let token = request
        .authorization
        .and_then(crate::door_auth::bearer_token);
    let Some(token) =
        token.filter(|token| verify_control_token(&declared.key, &plane.session_id, token))
    else {
        let refusal = Refusal::new(
            401,
            "unauthenticated",
            "the control surface takes this session's control token as Authorization: Bearer",
        );
        plane.record_refused(&refusal, None).await;
        return refusal.response();
    };
    let auth = Authenticated {
        token_id: mac_token::token_id(token),
        state: &declared.state,
    };

    let outcome = serve_authenticated(plane, &auth, &request, body).await;
    match outcome {
        Ok(response) => response,
        Err(refusal) => {
            plane.record_refused(&refusal, Some(&auth)).await;
            refusal.response()
        }
    }
}

async fn serve_authenticated<'r, R: AsyncRead + Unpin>(
    plane: &ControlPlane,
    auth: &Authenticated<'_>,
    request: &ControlRequest<'r>,
    body: &mut R,
) -> Result<ResourceResponse, Refusal<'r>> {
    let path = request.path.split('?').next().unwrap_or_default();
    let not_found = || Refusal::new(404, "unknown_path", "no control resource at this path");
    let wrong_method = |allow: &'static str, target: Option<Target<'r>>| Refusal {
        status: 405,
        reason: "method_not_allowed",
        message: format!("this control resource answers {allow}"),
        target,
        allow: Some(allow),
    };

    if path == CONTROL_PATH {
        if request.method != "GET" {
            return Err(wrong_method("GET", None));
        }
        return Ok(ok(listing(&plane.session_id, auth.state)));
    }

    if let Some(name) = path.strip_prefix(SETTINGS_PREFIX) {
        let target = Target::setting(name);
        let Some(setting) = ControllableSetting::from_wire_name(name)
            .filter(|setting| auth.state.setting_value(*setting).is_some())
        else {
            return Err(Refusal::new(
                404,
                "undeclared_setting",
                "this capsule's control: block does not declare that setting",
            )
            .naming(target));
        };
        if request.method != "PUT" {
            return Err(wrong_method("PUT", Some(target)));
        }
        let value = read_setting_body(request, body)
            .await
            .map_err(|refusal| refusal.naming(target))?;
        let value = parse_setting_value(setting, &value)
            .map_err(|message| Refusal::new(422, "invalid_value", message).naming(target))?;
        let change_id = crate::trace::new_event_id();
        let previous = auth
            .state
            .change_setting(setting, value, change_id.clone())
            .ok_or_else(not_found)?;
        plane
            .record_change(ControlChange {
                event_id: change_id,
                token_id: &auth.token_id,
                kind: "setting",
                name: setting.wire_name(),
                action: "set",
                previous: Some(previous.to_json()),
                value: Some(value.to_json()),
                applies_from: Some(APPLIES_FROM),
                replaced: None,
            })
            .await;
        return Ok(ok(json!({
            "name": setting.wire_name(),
            "previous": previous.to_json(),
            "value": value.to_json(),
            "applies_from": APPLIES_FROM,
        })));
    }

    if let Some(name) = path.strip_prefix(SECRETS_PREFIX) {
        let target = Target::secret(name);
        if !auth.state.secrets.declares(name) {
            return Err(Refusal::new(
                404,
                "undeclared_secret",
                "this capsule's control: block does not declare that secret",
            )
            .naming(target));
        }
        if !matches!(request.method, "PUT" | "DELETE") {
            return Err(wrong_method("PUT, DELETE", Some(target)));
        }
        if !request.peer.is_some_and(is_loopback_peer) {
            return Err(Refusal::new(
                403,
                "not_loopback",
                "a secret is accepted only over a loopback connection, because the door has no TLS",
            )
            .naming(target));
        }
        if request.method == "DELETE" {
            let _ = auth.state.secrets.forget(name);
            plane
                .record_change(ControlChange {
                    event_id: crate::trace::new_event_id(),
                    token_id: &auth.token_id,
                    kind: "secret",
                    name,
                    action: "forget",
                    previous: None,
                    value: None,
                    applies_from: None,
                    replaced: None,
                })
                .await;
            return Ok(ok(json!({"name": name, "set": false})));
        }
        let value = read_secret_body(request, body)
            .await
            .map_err(|refusal| refusal.naming(target))?;
        let replaced = auth.state.secrets.set(name, value).ok_or_else(not_found)?;
        plane
            .record_change(ControlChange {
                event_id: crate::trace::new_event_id(),
                token_id: &auth.token_id,
                kind: "secret",
                name,
                action: "set",
                previous: None,
                value: None,
                applies_from: None,
                replaced: Some(replaced),
            })
            .await;
        return Ok(ok(json!({"name": name, "set": true, "replaced": replaced})));
    }

    Err(not_found())
}

fn listing(session_id: &str, state: &ControlState) -> Value {
    let settings: Vec<Value> = state
        .settings_listing()
        .into_iter()
        .map(|(setting, value)| json!({"name": setting.wire_name(), "value": value.to_json()}))
        .collect();
    let secrets: Vec<Value> = state
        .secrets
        .listing()
        .into_iter()
        .map(|(name, set)| json!({"name": name, "set": set}))
        .collect();
    json!({"session_id": session_id, "settings": settings, "secrets": secrets})
}

/// The declared length checked against `max`, before anything is allocated.
fn checked_length(request: &ControlRequest<'_>, max: usize) -> Result<usize, Refusal<'static>> {
    if request.content_length > max {
        return Err(Refusal::new(
            413,
            "body_too_large",
            format!("the body is longer than the {max} bytes this resource reads"),
        ));
    }
    if request.content_length == 0 {
        return Err(Refusal::new(
            422,
            "missing_body",
            "a PUT carries its value as a body with a content-length",
        ));
    }
    Ok(request.content_length)
}

async fn read_exact_body<R: AsyncRead + Unpin>(
    body: &mut R,
    length: usize,
) -> Result<Vec<u8>, Refusal<'static>> {
    let mut bytes = vec![0u8; length];
    match body.read_exact(&mut bytes).await {
        Ok(_) => Ok(bytes),
        Err(_) => {
            // Wrapped so the partial bytes are overwritten when dropped.
            drop(SecretValue::from_bytes(bytes));
            Err(Refusal::new(
                400,
                "truncated_body",
                "the connection ended before content-length bytes of body arrived",
            ))
        }
    }
}

async fn read_setting_body<R: AsyncRead + Unpin>(
    request: &ControlRequest<'_>,
    body: &mut R,
) -> Result<Value, Refusal<'static>> {
    let length = checked_length(request, SETTINGS_BODY_MAX_BYTES)?;
    if !request.is_json {
        return Err(Refusal::new(
            415,
            "unsupported_media_type",
            "a setting is sent as content-type: application/json",
        ));
    }
    let bytes = read_exact_body(body, length).await?;
    let invalid = || {
        Refusal::new(
            422,
            "invalid_value",
            "a setting body is a JSON object {\"value\": <value>}",
        )
    };
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    parsed.get("value").cloned().ok_or_else(invalid)
}

async fn read_secret_body<R: AsyncRead + Unpin>(
    request: &ControlRequest<'_>,
    body: &mut R,
) -> Result<SecretValue, Refusal<'static>> {
    let length = checked_length(request, SECRET_BODY_MAX_BYTES)?;
    let value = SecretValue::from_bytes(read_exact_body(body, length).await?);
    if !value.is_header_safe() {
        return Err(Refusal::new(
            422,
            "invalid_value",
            "a secret is one line of visible ASCII and spaces, with no control bytes; the value \
             is not echoed",
        ));
    }
    Ok(value)
}

/// Store access for other modules' tests, which have no control plane to go through.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{InjectedSecrets, SecretValue};

    pub(crate) fn set(secrets: &InjectedSecrets, name: &str, value: &str) {
        secrets
            .set(name, SecretValue::from_bytes(value.as_bytes().to_vec()))
            .expect("declared");
    }

    pub(crate) fn forget(secrets: &InjectedSecrets, name: &str) {
        secrets.forget(name).expect("declared");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const SESSION: &str = "ses_0199aaaabbbbccccddddeeeeffff0000";
    const MARKER: &str = "ctl-marker-7f3e2a91";

    fn inference(max_tokens: Option<u32>) -> InferenceConfig {
        InferenceConfig {
            transport: "http".to_string(),
            model: "m".to_string(),
            driver: None,
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens,
            max_session_tokens: None,
        }
    }

    fn config() -> ControlConfig {
        ControlConfig {
            settings: vec![ControllableSetting::InferenceMaxTokens],
            secrets: vec!["CARD_TOKEN".to_string()],
        }
    }

    struct Fixture {
        plane: ControlPlane,
        token: String,
        state: Arc<ControlState>,
        workdir: tempfile::TempDir,
    }

    impl Fixture {
        async fn new() -> Self {
            let workdir = tempfile::tempdir().unwrap();
            let state = Arc::new(ControlState::new(&config(), &inference(Some(4096))));
            let (plane, token) =
                ControlPlane::declared(SESSION.to_string(), Arc::clone(&state)).unwrap();
            let appender = ResourceTraceAppender::open(
                workdir.path(),
                SESSION.to_string(),
                "evt_session".to_string(),
            )
            .await
            .unwrap();
            plane.attach_trace(Arc::new(appender));
            Self {
                plane,
                token,
                state,
                workdir,
            }
        }

        fn bearer(&self) -> String {
            format!("Bearer {}", self.token)
        }

        fn trace(&self) -> String {
            std::fs::read_to_string(self.workdir.path().join("trace.jsonl")).unwrap_or_default()
        }

        fn trace_events(&self) -> Vec<Value> {
            self.trace()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    struct Sent<'a> {
        method: &'a str,
        path: &'a str,
        authorization: Option<&'a str>,
        peer: IpAddr,
        is_json: bool,
        body: &'a [u8],
        content_length: Option<usize>,
    }

    impl<'a> Sent<'a> {
        fn new(method: &'a str, path: &'a str, authorization: Option<&'a str>) -> Self {
            Self {
                method,
                path,
                authorization,
                peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
                is_json: false,
                body: b"",
                content_length: None,
            }
        }

        fn body(mut self, body: &'a [u8]) -> Self {
            self.body = body;
            self
        }

        fn json(mut self) -> Self {
            self.is_json = true;
            self
        }

        fn peer(mut self, peer: IpAddr) -> Self {
            self.peer = peer;
            self
        }

        fn length(mut self, length: usize) -> Self {
            self.content_length = Some(length);
            self
        }

        /// The response, and how many body bytes the handler left unread.
        async fn to(self, plane: &ControlPlane) -> (ResourceResponse, usize) {
            let mut reader: &[u8] = self.body;
            let response = handle_control_request(
                plane,
                ControlRequest {
                    method: self.method,
                    path: self.path,
                    authorization: self.authorization,
                    peer: Some(self.peer),
                    content_length: self.content_length.unwrap_or(self.body.len()),
                    is_json: self.is_json,
                },
                &mut reader,
            )
            .await;
            (response, reader.len())
        }
    }

    fn body_json(response: &ResourceResponse) -> Value {
        serde_json::from_slice(&response.body).unwrap()
    }

    fn header<'r>(response: &'r ResourceResponse, name: &str) -> Option<&'r str> {
        response
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn loopback_peers_are_classified() {
        for loopback in ["127.0.0.1", "127.255.0.1", "::1", "::ffff:127.0.0.1"] {
            assert!(
                is_loopback_peer(loopback.parse().unwrap()),
                "{loopback} is loopback"
            );
        }
        for remote in ["10.0.0.1", "::ffff:10.0.0.1", "fe80::1", "0.0.0.0", "::"] {
            assert!(
                !is_loopback_peer(remote.parse().unwrap()),
                "{remote} is not loopback"
            );
        }
    }

    #[test]
    fn a_token_verifies_only_for_its_own_key_and_session() {
        let key = MintKey::generate().unwrap();
        let token = mint_control_token(&key, SESSION).unwrap();
        assert!(token.starts_with("ctl1."));
        assert!(verify_control_token(&key, SESSION, &token));
        assert!(!verify_control_token(&key, "ses_other", &token));
        assert!(!verify_control_token(
            &MintKey::generate().unwrap(),
            SESSION,
            &token
        ));
        assert!(!verify_control_token(&key, SESSION, &token.to_uppercase()));
        let swapped = token.replacen("ctl1.", "pfh1.", 1);
        assert!(!verify_control_token(&key, SESSION, &swapped));
    }

    #[test]
    fn secrets_and_their_stores_debug_print_redacted() {
        let value = SecretValue::from_bytes(MARKER.as_bytes().to_vec());
        assert_eq!(format!("{value:?}"), "<redacted>");

        let store = InjectedSecrets::new(&["CARD_TOKEN".to_string()]);
        store.set("CARD_TOKEN", value).unwrap();
        let printed = format!("{store:?}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(!printed.contains(MARKER), "{printed}");

        let state = ControlState::new(&config(), &inference(None));
        state
            .secrets
            .set(
                "CARD_TOKEN",
                SecretValue::from_bytes(MARKER.as_bytes().to_vec()),
            )
            .unwrap();
        let printed = format!("{state:?}");
        assert!(!printed.contains(MARKER), "{printed}");

        let credential = crate::gateway_credential::GatewayCredential::injected(
            "card-api",
            "CARD_TOKEN",
            state.secrets(),
        );
        let printed = format!("{credential:?}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(!printed.contains(MARKER), "{printed}");
    }

    #[test]
    fn zeroizing_a_secret_clears_every_byte() {
        let mut value = SecretValue::from_bytes(MARKER.as_bytes().to_vec());
        value.zeroize();
        assert!(value.0.iter().all(|byte| *byte == 0));
    }

    /// The secret type is the one place a value lives, so it may derive nothing and carry no trait
    /// impl beyond `Debug` and `Drop`: any other would be a route out of it.
    #[test]
    fn the_secret_type_derives_nothing_and_implements_only_debug_and_drop() {
        let source = include_str!("control_plane.rs");
        let (before, _) = source
            .split_once("pub(crate) struct SecretValue(")
            .expect("SecretValue is declared");
        let preceding: Vec<&str> = before
            .lines()
            .rev()
            .take_while(|line| {
                line.trim_start().starts_with("///") || line.trim().starts_with("#[")
            })
            .collect();
        assert!(
            preceding.iter().all(|line| !line.contains("derive")),
            "SecretValue carries a derive: {preceding:?}"
        );
        let impls: Vec<&str> = source
            .lines()
            .filter(|line| line.starts_with("impl") && line.contains(" for SecretValue"))
            .collect();
        assert_eq!(
            impls,
            vec![
                "impl Drop for SecretValue {",
                "impl std::fmt::Debug for SecretValue {"
            ]
        );
    }

    #[test]
    fn launch_value_is_the_manifest_value_or_the_default() {
        let state = ControlState::new(&config(), &inference(None));
        assert_eq!(
            state.setting_value(ControllableSetting::InferenceMaxTokens),
            Some(SettingValue::Tokens(
                crate::agent::DEFAULT_MAX_OUTPUT_TOKENS
            ))
        );
    }

    #[test]
    fn the_first_call_to_read_a_changed_value_names_the_change() {
        let state = ControlState::new(&config(), &inference(Some(4096)));
        let setting = ControllableSetting::InferenceMaxTokens;
        let first = state.for_call(setting).unwrap();
        assert_eq!(first.value, SettingValue::Tokens(4096));
        assert_eq!(first.applies_change, None);

        state.change_setting(setting, SettingValue::Tokens(1234), "evt_a".to_string());
        state.change_setting(setting, SettingValue::Tokens(2048), "evt_b".to_string());
        let changed = state.for_call(setting).unwrap();
        assert_eq!(changed.value, SettingValue::Tokens(2048));
        assert_eq!(changed.applies_change.as_deref(), Some("evt_b"));
        assert_eq!(state.for_call(setting).unwrap().applies_change, None);

        state.change_setting(setting, SettingValue::Tokens(2048), "evt_c".to_string());
        assert_eq!(state.for_call(setting).unwrap().applies_change, None);
    }

    #[tokio::test]
    async fn an_undeclared_plane_answers_404_and_reads_nothing() {
        let plane = ControlPlane::undeclared(SESSION.to_string());
        let (response, unread) = Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some("Bearer x"))
            .body(MARKER.as_bytes())
            .to(&plane)
            .await;
        assert_eq!(response.status, 404);
        assert_eq!(unread, MARKER.len());
    }

    #[tokio::test]
    async fn a_wrong_token_is_401_before_the_body_is_read_and_names_nothing() {
        let fixture = Fixture::new().await;
        let wrong_case = fixture.bearer().to_uppercase();
        for authorization in [
            None,
            Some("Bearer garbage"),
            Some(wrong_case.as_str()),
            Some("Basic Zm9vOmJhcg=="),
        ] {
            let (response, unread) = Sent::new("PUT", "/control/secrets/CARD_TOKEN", authorization)
                .body(MARKER.as_bytes())
                .to(&fixture.plane)
                .await;
            assert_eq!(response.status, 401);
            assert_eq!(
                header(&response, "www-authenticate"),
                Some("Bearer realm=\"murmur-control\"")
            );
            assert_eq!(unread, MARKER.len(), "the body was read");
            assert!(!String::from_utf8_lossy(&response.body).contains("CARD_TOKEN"));
        }
        let events = fixture.trace_events();
        assert_eq!(events.len(), 4);
        for event in events {
            assert_eq!(event["event_type"], "control_refused");
            assert_eq!(event["status"], 401);
            assert!(event.get("name").is_none(), "{event}");
            assert!(event.get("token_id").is_none(), "{event}");
        }
        assert!(!fixture.trace().contains(MARKER));
    }

    #[tokio::test]
    async fn a_setting_change_answers_and_records_previous_and_value() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        let (response, _) = Sent::new(
            "PUT",
            "/control/settings/inference.max_tokens",
            Some(&bearer),
        )
        .json()
        .body(br#"{"value": 1234}"#)
        .to(&fixture.plane)
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            body_json(&response),
            json!({"name": "inference.max_tokens", "previous": 4096, "value": 1234, "applies_from": "next_inference_call"})
        );
        let events = fixture.trace_events();
        assert_eq!(events.len(), 1);
        let change = &events[0];
        assert_eq!(change["event_type"], "control_change");
        assert_eq!(change["principal"], "controller");
        assert_eq!(change["token_id"], mac_token::token_id(&fixture.token));
        assert_eq!(change["kind"], "setting");
        assert_eq!(change["action"], "set");
        assert_eq!(change["previous"], 4096);
        assert_eq!(change["value"], 1234);
        let applied = fixture
            .state
            .for_call(ControllableSetting::InferenceMaxTokens)
            .unwrap();
        assert_eq!(
            applied.applies_change.as_deref(),
            change["event_id"].as_str()
        );
    }

    #[tokio::test]
    async fn listing_names_settings_and_secrets_and_never_a_value() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        let (response, _) = Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
            .body(MARKER.as_bytes())
            .to(&fixture.plane)
            .await;
        assert_eq!(
            body_json(&response),
            json!({"name": "CARD_TOKEN", "set": true, "replaced": false})
        );
        let (response, _) = Sent::new("GET", "/control", Some(&bearer))
            .to(&fixture.plane)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            body_json(&response),
            json!({
                "session_id": SESSION,
                "settings": [{"name": "inference.max_tokens", "value": 4096}],
                "secrets": [{"name": "CARD_TOKEN", "set": true}],
            })
        );
        assert!(!String::from_utf8_lossy(&response.body).contains(MARKER));
        assert_eq!(
            fixture.state.secrets.value("CARD_TOKEN").as_deref(),
            Some(MARKER)
        );
        let (response, _) = Sent::new("DELETE", "/control/secrets/CARD_TOKEN", Some(&bearer))
            .to(&fixture.plane)
            .await;
        assert_eq!(
            body_json(&response),
            json!({"name": "CARD_TOKEN", "set": false})
        );
        assert_eq!(fixture.state.secrets.value("CARD_TOKEN"), None);
        assert!(!fixture.trace().contains(MARKER));
    }

    /// Every way a secret request can be refused, each driven with the marker as its body: neither
    /// the response nor the trace line carries it.
    #[tokio::test]
    async fn every_secret_refusal_leaks_nothing_of_the_body() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        let remote = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let with_cr = format!("{MARKER}\r");
        let with_lf = format!("{MARKER}\nmore");
        let with_nul = format!("{MARKER}\0");
        let oversized = format!("{MARKER}{}", "x".repeat(SECRET_BODY_MAX_BYTES));
        let cases: Vec<(Sent<'_>, u16, &str)> = vec![
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some("Bearer nope"))
                    .body(MARKER.as_bytes()),
                401,
                "unauthenticated",
            ),
            (
                Sent::new("PUT", "/control/secrets/OTHER", Some(&bearer)).body(MARKER.as_bytes()),
                404,
                "undeclared_secret",
            ),
            (
                Sent::new("POST", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(MARKER.as_bytes()),
                405,
                "method_not_allowed",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(MARKER.as_bytes())
                    .peer(remote),
                403,
                "not_loopback",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(oversized.as_bytes()),
                413,
                "body_too_large",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer)),
                422,
                "missing_body",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(with_cr.as_bytes()),
                422,
                "invalid_value",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(with_lf.as_bytes()),
                422,
                "invalid_value",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(with_nul.as_bytes()),
                422,
                "invalid_value",
            ),
            (
                Sent::new("PUT", "/control/secrets/CARD_TOKEN", Some(&bearer))
                    .body(MARKER.as_bytes())
                    .length(MARKER.len() + 10),
                400,
                "truncated_body",
            ),
        ];
        let count = cases.len();
        for (sent, status, reason) in cases {
            let (response, _) = sent.to(&fixture.plane).await;
            assert_eq!(response.status, status, "{reason}");
            let text = String::from_utf8_lossy(&response.body);
            assert!(!text.contains(MARKER), "{reason}: {text}");
            let error = body_json(&response)["error"].as_str().unwrap().to_string();
            assert!(!error.is_empty());
        }
        let events = fixture.trace_events();
        assert_eq!(events.len(), count);
        assert!(events.iter().all(|e| e["event_type"] == "control_refused"));
        assert!(!fixture.trace().contains(MARKER));
        assert_eq!(fixture.state.secrets.value("CARD_TOKEN"), None);
    }

    #[tokio::test]
    async fn setting_values_outside_the_range_are_422_and_change_nothing() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        for body in [
            r#"{"value": 0}"#,
            r#"{"value": -1}"#,
            r#"{"value": "2048"}"#,
            r#"{"value": 4294967296}"#,
            r#"{"value": 1.5}"#,
            r#"{"other": 5}"#,
            "not json",
        ] {
            let (response, _) = Sent::new(
                "PUT",
                "/control/settings/inference.max_tokens",
                Some(&bearer),
            )
            .json()
            .body(body.as_bytes())
            .to(&fixture.plane)
            .await;
            assert_eq!(response.status, 422, "{body}");
        }
        let (response, _) = Sent::new(
            "PUT",
            "/control/settings/inference.max_tokens",
            Some(&bearer),
        )
        .body(br#"{"value": 5}"#)
        .to(&fixture.plane)
        .await;
        assert_eq!(response.status, 415);
        let (response, unread) = Sent::new(
            "PUT",
            "/control/settings/inference.max_tokens",
            Some(&bearer),
        )
        .json()
        .body(&[b' '; SETTINGS_BODY_MAX_BYTES + 1])
        .to(&fixture.plane)
        .await;
        assert_eq!(response.status, 413);
        assert_eq!(unread, SETTINGS_BODY_MAX_BYTES + 1);
        assert_eq!(
            fixture
                .state
                .setting_value(ControllableSetting::InferenceMaxTokens),
            Some(SettingValue::Tokens(4096))
        );
        for event in fixture.trace_events() {
            assert_eq!(event["event_type"], "control_refused");
            assert_eq!(event["name"], "inference.max_tokens");
            assert_eq!(event["kind"], "setting");
        }
    }

    #[tokio::test]
    async fn a_settings_change_is_accepted_from_a_remote_peer() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        let (response, _) = Sent::new(
            "PUT",
            "/control/settings/inference.max_tokens",
            Some(&bearer),
        )
        .json()
        .body(br#"{"value": 2048}"#)
        .peer(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)))
        .to(&fixture.plane)
        .await;
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_are_refused_and_recorded() {
        let fixture = Fixture::new().await;
        let bearer = fixture.bearer();
        for (method, path, status) in [
            ("GET", "/control/nothing", 404),
            ("GET", "/control/settings/inference.model", 404),
            ("PUT", "/control/settings/context.max_tokens", 404),
            ("POST", "/control", 405),
            ("GET", "/control/settings/inference.max_tokens", 405),
        ] {
            let (response, _) = Sent::new(method, path, Some(&bearer))
                .to(&fixture.plane)
                .await;
            assert_eq!(response.status, status, "{method} {path}");
            if status == 405 {
                assert!(header(&response, "allow").is_some());
            }
        }
        let events = fixture.trace_events();
        assert_eq!(events.len(), 5);
        assert_eq!(events[4]["name"], "inference.max_tokens");
    }
}
