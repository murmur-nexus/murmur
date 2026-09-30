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
    driver_choice::{DriverChoice, DriverChoices},
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

    /// Whether a value is held for `name`. Reads nothing out and records nothing, so a check made
    /// before a switch leaves no trace of its own.
    pub(crate) fn holds(&self, name: &str) -> bool {
        self.with_slot(name, |slot| slot.is_some()).unwrap_or(false)
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

/// Who changes a setting: the holder of the control token over [`CONTROL_PATH`], or the agent
/// through the runtime-provided tool its grant writes. Recorded as `principal` on every
/// `control_change` and `control_refused` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Principal {
    Controller,
    Agent,
}

impl Principal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::Agent => "agent",
        }
    }
}

/// A setting's value as the plane validated it. One variant per value shape a
/// [`ControllableSetting`] takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingValue {
    /// A token count, `1..=u32::MAX`.
    Tokens(u32),
    /// A driver choice, by its index in the session's [`DriverChoices`].
    Driver(usize),
}

impl SettingValue {
    /// The value as the surface and the trace spell it: a token count as a number, a driver
    /// choice by its name.
    fn to_json(self, choices: &DriverChoices) -> Value {
        match self {
            Self::Tokens(count) => json!(count),
            Self::Driver(index) => json!(choices.get(index).map(|choice| choice.name.as_str())),
        }
    }

    /// The token count, for a setting whose value is one.
    pub(crate) fn tokens(self) -> Option<u32> {
        match self {
            Self::Tokens(count) => Some(count),
            Self::Driver(_) => None,
        }
    }

    /// The driver choice's index, for `inference.driver`.
    pub(crate) fn driver(self) -> Option<usize> {
        match self {
            Self::Driver(index) => Some(index),
            Self::Tokens(_) => None,
        }
    }
}

/// The value `setting` starts the session with: what the manifest resolved to. A driver setting
/// always starts on the primary, so nothing a controller or the agent chose survives a restart.
fn launch_value(setting: ControllableSetting, inference: &InferenceConfig) -> SettingValue {
    match setting {
        ControllableSetting::InferenceMaxTokens => SettingValue::Tokens(
            inference
                .max_tokens
                .unwrap_or(crate::agent::DEFAULT_MAX_OUTPUT_TOKENS),
        ),
        ControllableSetting::InferenceDriver => SettingValue::Driver(0),
    }
}

/// A change refused before anything changed: its HTTP status, the trace's reason word, and the
/// one sentence the caller is told. Never quotes a credential value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangeRefused {
    pub(crate) status: u16,
    pub(crate) reason: &'static str,
    pub(crate) message: String,
}

impl ChangeRefused {
    fn new(status: u16, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            message: message.into(),
        }
    }
}

/// `value` as `setting` takes it, or why it does not.
fn parse_setting_value(
    setting: ControllableSetting,
    value: &Value,
    choices: &DriverChoices,
) -> Result<SettingValue, ChangeRefused> {
    match setting {
        ControllableSetting::InferenceMaxTokens => value
            .as_u64()
            .and_then(|count| u32::try_from(count).ok())
            .filter(|count| *count >= 1)
            .map(SettingValue::Tokens)
            .ok_or_else(|| {
                ChangeRefused::new(
                    422,
                    "invalid_value",
                    format!(
                        "{} takes an integer from 1 to {}",
                        setting.wire_name(),
                        u32::MAX
                    ),
                )
            }),
        ControllableSetting::InferenceDriver => value
            .as_str()
            .and_then(|name| choices.index_of(name))
            .map(SettingValue::Driver)
            .ok_or_else(|| {
                ChangeRefused::new(
                    422,
                    "undeclared_driver",
                    format!(
                        "{} takes the name of a declared driver choice; this capsule declares: {}",
                        setting.wire_name(),
                        choices.names()
                    ),
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
    /// Whether `control.settings` lists the setting, so a controller may change it.
    controller: bool,
    /// Whether `control.agent_settings` lists the setting, so the agent may change it.
    agent: bool,
}

impl SettingSlot {
    fn permits(&self, principal: Principal) -> bool {
        match principal {
            Principal::Controller => self.controller,
            Principal::Agent => self.agent,
        }
    }
}

/// An accepted setting change, as the caller records and answers it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AcceptedChange {
    /// The `event_id` its `control_change` is written under, and that `control_applied` names.
    pub(crate) change_id: String,
    pub(crate) previous: Value,
    pub(crate) value: Value,
}

/// What a controller or the agent has changed on this session, read by the agent loop and the
/// gateways.
///
/// One lock guards the settings, and every check that depends on which driver choice is in use is
/// made under it: a switch checks the choice's credential and a forget checks the choice in use,
/// so neither can pass against a state the other is changing.
pub(crate) struct ControlState {
    settings: Mutex<Vec<SettingSlot>>,
    secrets: Arc<InjectedSecrets>,
    choices: DriverChoices,
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
    /// resolves it to. `choices` is the session's driver choice table as staging left it, and
    /// `secrets` the store every injected gateway credential reads.
    pub(crate) fn new(
        config: &ControlConfig,
        inference: &InferenceConfig,
        choices: DriverChoices,
        secrets: Arc<InjectedSecrets>,
    ) -> Self {
        let declared = config
            .settings
            .iter()
            .chain(
                config
                    .agent_settings
                    .iter()
                    .filter(|setting| !config.settings.contains(setting)),
            )
            .map(|setting| {
                let value = launch_value(*setting, inference);
                SettingSlot {
                    setting: *setting,
                    value,
                    change_id: None,
                    last_used: value,
                    controller: config.controller_may_set(*setting),
                    agent: config.agent_may_set(*setting),
                }
            })
            .collect();
        Self {
            settings: Mutex::new(declared),
            secrets,
            choices,
        }
    }

    /// The session's driver choices.
    pub(crate) fn choices(&self) -> &DriverChoices {
        &self.choices
    }

    /// Whether a controller has anything to call: a setting it may change or a secret it may
    /// supply. A session whose `control:` block grants only the agent mints no control token and
    /// answers `404` under [`CONTROL_PATH`].
    pub(crate) fn has_controller_surface(&self) -> bool {
        self.lock_settings().iter().any(|slot| slot.controller)
            || !self.secrets.listing().is_empty()
    }

    fn lock_settings(&self) -> std::sync::MutexGuard<'_, Vec<SettingSlot>> {
        self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `principal` may change `setting` on this session.
    pub(crate) fn permits(&self, principal: Principal, setting: ControllableSetting) -> bool {
        self.lock_settings()
            .iter()
            .any(|slot| slot.setting == setting && slot.permits(principal))
    }

    /// Whether a switch to `choice` would be accepted now: its credential resolved at launch and,
    /// for an injected one, a value is held.
    fn choice_available(&self, choice: &DriverChoice) -> bool {
        choice.staged()
            && choice
                .injected_secret
                .as_deref()
                .is_none_or(|name| self.secrets.holds(name))
    }

    /// Why a switch to `choice` is refused, or `None` when it would be accepted.
    fn unresolvable(&self, choice: &DriverChoice) -> Option<ChangeRefused> {
        if let Some(credential) = &choice.unresolved_credential {
            return Some(ChangeRefused::new(
                409,
                "credential_unresolvable",
                format!(
                    "driver choice '{}' cannot be selected: its credential {credential} was not \
                     found when the capsule launched",
                    choice.name
                ),
            ));
        }
        let name = choice
            .injected_secret
            .as_deref()
            .filter(|name| !self.secrets.holds(name))?;
        Some(ChangeRefused::new(
            409,
            "credential_unresolvable",
            format!(
                "driver choice '{}' cannot be selected: its credential {name} is a control.secrets \
                 name with no value injected; supply it with `mur control secret {name}` first",
                choice.name
            ),
        ))
    }

    /// Validates `requested` for `setting` and applies it for `principal`, returning the change
    /// the caller records. Everything is decided under the settings lock, so a driver choice's
    /// credential is checked against the same state a forget is.
    pub(crate) fn request_change(
        &self,
        principal: Principal,
        setting: ControllableSetting,
        requested: &Value,
    ) -> Result<AcceptedChange, ChangeRefused> {
        let value = parse_setting_value(setting, requested, &self.choices)?;
        let mut settings = self.lock_settings();
        let Some(slot) = settings
            .iter_mut()
            .find(|slot| slot.setting == setting && slot.permits(principal))
        else {
            return Err(ChangeRefused::new(
                404,
                "undeclared_setting",
                "this capsule's control: block does not declare that setting",
            ));
        };
        if let Some(refusal) = value
            .driver()
            .and_then(|index| self.choices.get(index))
            .and_then(|choice| self.unresolvable(choice))
        {
            return Err(refusal);
        }
        let change_id = crate::trace::new_event_id();
        let previous = slot.value;
        slot.value = value;
        slot.change_id = Some(change_id.clone());
        Ok(AcceptedChange {
            change_id,
            previous: previous.to_json(&self.choices),
            value: value.to_json(&self.choices),
        })
    }

    /// Drops the value held for secret `name`, refused with `409 in_use` while `name` keys the
    /// driver choice in use. `Some(was_set)` when `name` is declared.
    fn forget_secret(&self, name: &str) -> Result<Option<bool>, ChangeRefused> {
        let settings = self.lock_settings();
        let in_use = settings
            .iter()
            .find(|slot| slot.setting == ControllableSetting::InferenceDriver)
            .and_then(|slot| slot.value.driver())
            .and_then(|index| self.choices.get(index))
            .filter(|choice| choice.injected_secret.as_deref() == Some(name));
        if let Some(choice) = in_use {
            return Err(ChangeRefused::new(
                409,
                "in_use",
                format!(
                    "{name} keys driver choice '{}', which is in use; switch inference.driver to \
                     another choice before forgetting it",
                    choice.name
                ),
            ));
        }
        let forgotten = self.secrets.forget(name);
        drop(settings);
        Ok(forgotten)
    }

    /// The value `setting` holds now, without marking it read by any call. `None` for a setting
    /// this session does not declare.
    pub(crate) fn selected(&self, setting: ControllableSetting) -> Option<SettingValue> {
        self.lock_settings()
            .iter()
            .find(|slot| slot.setting == setting)
            .map(|slot| slot.value)
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
        let settings = self.lock_settings();
        let names = |principal: Principal| {
            settings
                .iter()
                .filter(|slot| slot.permits(principal))
                .map(|slot| slot.setting.wire_name())
                .collect()
        };
        crate::trace::SessionControl {
            settings: names(Principal::Controller),
            secrets: self
                .secrets
                .listing()
                .into_iter()
                .map(|(name, _)| name)
                .collect(),
            agent_settings: names(Principal::Agent),
        }
    }

    /// The `settings` array `GET /control` lists: every setting a controller may change, with its
    /// value, and for `inference.driver` every choice and whether a switch to it would be
    /// accepted now.
    fn settings_listing(&self) -> Vec<Value> {
        let settings = self.lock_settings();
        settings
            .iter()
            .filter(|slot| slot.controller)
            .map(|slot| {
                let mut entry = json!({
                    "name": slot.setting.wire_name(),
                    "value": slot.value.to_json(&self.choices),
                });
                if slot.setting == ControllableSetting::InferenceDriver {
                    entry["choices"] =
                        json!(self.choices.listing(|choice| self.choice_available(choice)));
                }
                entry
            })
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
            .write_control_refused(
                Principal::Controller,
                refusal.status,
                refusal.reason,
                kind,
                name,
                token_id,
            )
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

impl From<ChangeRefused> for Refusal<'_> {
    fn from(refused: ChangeRefused) -> Self {
        Self::new(refused.status, refused.reason, refused.message)
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
            .filter(|setting| auth.state.permits(Principal::Controller, *setting))
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
        let change = auth
            .state
            .request_change(Principal::Controller, setting, &value)
            .map_err(|refused| Refusal::from(refused).naming(target))?;
        let answer = json!({
            "name": setting.wire_name(),
            "previous": change.previous,
            "value": change.value,
            "applies_from": APPLIES_FROM,
        });
        plane
            .record_change(ControlChange {
                event_id: change.change_id,
                principal: Principal::Controller,
                token_id: Some(&auth.token_id),
                kind: "setting",
                name: setting.wire_name(),
                action: "set",
                previous: Some(change.previous),
                value: Some(change.value),
                applies_from: Some(APPLIES_FROM),
                replaced: None,
            })
            .await;
        return Ok(ok(answer));
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
            auth.state
                .forget_secret(name)
                .map_err(|refused| Refusal::from(refused).naming(target))?;
            plane
                .record_change(ControlChange {
                    event_id: crate::trace::new_event_id(),
                    principal: Principal::Controller,
                    token_id: Some(&auth.token_id),
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
                principal: Principal::Controller,
                token_id: Some(&auth.token_id),
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
    let settings = state.settings_listing();
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
            tool_refresh: murmur_artifact::ToolRefresh::Compaction,
            alternates: Vec::new(),
        }
    }

    fn state_for(config: &ControlConfig, inference: &InferenceConfig) -> ControlState {
        ControlState::new(
            config,
            inference,
            DriverChoices::declared(inference),
            Arc::new(InjectedSecrets::new(&config.secrets)),
        )
    }

    fn config() -> ControlConfig {
        ControlConfig {
            settings: vec![ControllableSetting::InferenceMaxTokens],
            secrets: vec!["CARD_TOKEN".to_string()],
            agent_settings: Vec::new(),
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
            let state = Arc::new(state_for(&config(), &inference(Some(4096))));
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

        let state = state_for(&config(), &inference(None));
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
            Arc::clone(&state.secrets),
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
        let state = state_for(&config(), &inference(None));
        assert_eq!(
            state.selected(ControllableSetting::InferenceMaxTokens),
            Some(SettingValue::Tokens(
                crate::agent::DEFAULT_MAX_OUTPUT_TOKENS
            ))
        );
    }

    #[test]
    fn the_first_call_to_read_a_changed_value_names_the_change() {
        let state = state_for(&config(), &inference(Some(4096)));
        let setting = ControllableSetting::InferenceMaxTokens;
        let first = state.for_call(setting).unwrap();
        assert_eq!(first.value, SettingValue::Tokens(4096));
        assert_eq!(first.applies_change, None);

        let set = |value: u32| {
            state
                .request_change(Principal::Controller, setting, &json!(value))
                .unwrap()
                .change_id
        };
        set(1234);
        let second = set(2048);
        let changed = state.for_call(setting).unwrap();
        assert_eq!(changed.value, SettingValue::Tokens(2048));
        assert_eq!(changed.applies_change, Some(second));
        assert_eq!(state.for_call(setting).unwrap().applies_change, None);

        set(2048);
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
                .selected(ControllableSetting::InferenceMaxTokens),
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

    // ── the driver setting ───────────────────────────────────────────────────

    const ALT_KEY: &str = "ALT_KEY";

    /// A capsule on sonnet with alternates `haiku` (same driver), `gpt` (keyed by the injected
    /// `ALT_KEY`) and `mini` (whose credential was found nowhere), switchable by a controller.
    fn switchable(agent: bool) -> (ControlConfig, InferenceConfig, DriverChoices) {
        let mut inference = inference(Some(4096));
        inference.driver = Some(murmur_artifact::InferenceDriver {
            artifact: "anthropic".to_string(),
            config: None,
        });
        for (name, model, driver) in [
            ("haiku", "claude-haiku-4-5", "anthropic"),
            ("gpt", "gpt-5", "openai"),
            ("mini", "gpt-5-mini", "other"),
        ] {
            inference
                .alternates
                .push(murmur_artifact::InferenceAlternate {
                    name: name.to_string(),
                    model: model.to_string(),
                    driver: driver.to_string(),
                });
        }
        let mut choices = DriverChoices::declared(&inference);
        choices.record_credential("anthropic", "config", None, None);
        choices.record_credential("openai", "injected", Some(ALT_KEY), None);
        choices.record_credential("other", "none", None, Some("MINI_KEY"));
        let config = ControlConfig {
            settings: if agent {
                Vec::new()
            } else {
                vec![ControllableSetting::InferenceDriver]
            },
            secrets: vec![ALT_KEY.to_string()],
            agent_settings: if agent {
                vec![ControllableSetting::InferenceDriver]
            } else {
                Vec::new()
            },
        };
        (config, inference, choices)
    }

    async fn switchable_fixture(agent: bool) -> Fixture {
        let workdir = tempfile::tempdir().unwrap();
        let (config, inference, choices) = switchable(agent);
        let state = Arc::new(ControlState::new(
            &config,
            &inference,
            choices,
            Arc::new(InjectedSecrets::new(&config.secrets)),
        ));
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
        Fixture {
            plane,
            token,
            state,
            workdir,
        }
    }

    async fn put_driver(fixture: &Fixture, body: &str) -> ResourceResponse {
        let bearer = fixture.bearer();
        Sent::new("PUT", "/control/settings/inference.driver", Some(&bearer))
            .json()
            .body(body.as_bytes())
            .to(&fixture.plane)
            .await
            .0
    }

    fn selected_driver(fixture: &Fixture) -> Option<usize> {
        fixture
            .state
            .selected(ControllableSetting::InferenceDriver)
            .and_then(SettingValue::driver)
    }

    #[tokio::test]
    async fn driver_choice_a_controller_switch_answers_and_records_choice_names() {
        let fixture = switchable_fixture(false).await;
        let response = put_driver(&fixture, r#"{"value": "haiku"}"#).await;
        assert_eq!(response.status, 200);
        assert_eq!(
            body_json(&response),
            json!({"name": "inference.driver", "previous": "primary", "value": "haiku", "applies_from": "next_inference_call"})
        );
        let change = &fixture.trace_events()[0];
        assert_eq!(change["event_type"], "control_change");
        assert_eq!(change["principal"], "controller");
        assert_eq!(change["token_id"], mac_token::token_id(&fixture.token));
        assert_eq!(change["previous"], "primary");
        assert_eq!(change["value"], "haiku");
        let reading = fixture
            .state
            .for_call(ControllableSetting::InferenceDriver)
            .unwrap();
        assert_eq!(reading.value, SettingValue::Driver(1));
        assert_eq!(
            reading.applies_change.as_deref(),
            change["event_id"].as_str()
        );
    }

    #[tokio::test]
    async fn driver_choice_an_undeclared_name_is_422_and_changes_nothing() {
        let fixture = switchable_fixture(false).await;
        for body in [
            r#"{"value": "claude-opus"}"#,
            r#"{"value": "openai"}"#,
            r#"{"value": 3}"#,
            r#"{"value": null}"#,
        ] {
            let response = put_driver(&fixture, body).await;
            assert_eq!(response.status, 422, "{body}");
            let error = body_json(&response)["error"].as_str().unwrap().to_string();
            assert!(
                error.contains("primary, haiku, gpt, mini"),
                "{body}: {error}"
            );
        }
        assert_eq!(selected_driver(&fixture), Some(0));
        for event in fixture.trace_events() {
            assert_eq!(event["event_type"], "control_refused");
            assert_eq!(event["reason"], "undeclared_driver");
            assert_eq!(event["principal"], "controller");
            assert_eq!(event["name"], "inference.driver");
        }
    }

    /// An injected credential with nothing injected cannot be selected; once injected it can, and
    /// while its choice is in use it cannot be forgotten.
    #[tokio::test]
    async fn driver_choice_an_injected_credential_gates_the_switch_and_the_forget() {
        let fixture = switchable_fixture(false).await;
        let bearer = fixture.bearer();
        let response = put_driver(&fixture, r#"{"value": "gpt"}"#).await;
        assert_eq!(response.status, 409);
        let error = body_json(&response)["error"].as_str().unwrap().to_string();
        assert!(error.contains(ALT_KEY), "{error}");
        assert_eq!(selected_driver(&fixture), Some(0));

        let (response, _) = Sent::new("PUT", "/control/secrets/ALT_KEY", Some(&bearer))
            .body(MARKER.as_bytes())
            .to(&fixture.plane)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            put_driver(&fixture, r#"{"value": "gpt"}"#).await.status,
            200
        );
        assert_eq!(selected_driver(&fixture), Some(2));

        let (response, _) = Sent::new("DELETE", "/control/secrets/ALT_KEY", Some(&bearer))
            .to(&fixture.plane)
            .await;
        assert_eq!(response.status, 409);
        assert!(fixture.state.secrets.holds(ALT_KEY), "the secret is kept");

        assert_eq!(
            put_driver(&fixture, r#"{"value": "primary"}"#).await.status,
            200
        );
        let (response, _) = Sent::new("DELETE", "/control/secrets/ALT_KEY", Some(&bearer))
            .to(&fixture.plane)
            .await;
        assert_eq!(response.status, 200);
        assert!(!fixture.state.secrets.holds(ALT_KEY));

        let reasons: Vec<String> = fixture
            .trace_events()
            .iter()
            .filter(|event| event["event_type"] == "control_refused")
            .map(|event| event["reason"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(reasons, vec!["credential_unresolvable", "in_use"]);
        assert!(!fixture.trace().contains(MARKER));
    }

    #[tokio::test]
    async fn driver_choice_a_choice_staged_unavailable_is_409() {
        let fixture = switchable_fixture(false).await;
        let response = put_driver(&fixture, r#"{"value": "mini"}"#).await;
        assert_eq!(response.status, 409);
        let error = body_json(&response)["error"].as_str().unwrap().to_string();
        assert!(error.contains("MINI_KEY"), "{error}");
        assert_eq!(selected_driver(&fixture), Some(0));
    }

    #[tokio::test]
    async fn driver_choice_listing_names_every_choice_and_whether_it_can_be_selected() {
        let fixture = switchable_fixture(false).await;
        let bearer = fixture.bearer();
        let (response, _) = Sent::new("GET", "/control", Some(&bearer))
            .to(&fixture.plane)
            .await;
        assert_eq!(
            body_json(&response)["settings"],
            json!([{
                "name": "inference.driver",
                "value": "primary",
                "choices": [
                    {"name": "primary", "driver": "anthropic", "model": "m", "available": true},
                    {"name": "haiku", "driver": "anthropic", "model": "claude-haiku-4-5", "available": true},
                    {"name": "gpt", "driver": "openai", "model": "gpt-5", "available": false},
                    {"name": "mini", "driver": "other", "model": "gpt-5-mini", "available": false},
                ],
            }])
        );
    }

    /// A setting only the agent may change is undeclared to a controller, and a session granting
    /// only the agent has no controller surface.
    #[tokio::test]
    async fn driver_choice_an_agent_only_setting_is_undeclared_to_a_controller() {
        let fixture = switchable_fixture(true).await;
        assert!(
            fixture.state.has_controller_surface(),
            "ALT_KEY is a secret"
        );
        let response = put_driver(&fixture, r#"{"value": "haiku"}"#).await;
        assert_eq!(response.status, 404);
        assert_eq!(fixture.trace_events()[0]["reason"], "undeclared_setting");

        let change = fixture
            .state
            .request_change(
                Principal::Agent,
                ControllableSetting::InferenceDriver,
                &json!("haiku"),
            )
            .unwrap();
        assert_eq!(change.previous, json!("primary"));
        assert_eq!(change.value, json!("haiku"));
        let refused = fixture
            .state
            .request_change(
                Principal::Agent,
                ControllableSetting::InferenceDriver,
                &json!("nope"),
            )
            .unwrap_err();
        assert_eq!((refused.status, refused.reason), (422, "undeclared_driver"));
        assert_eq!(
            fixture
                .state
                .request_change(
                    Principal::Controller,
                    ControllableSetting::InferenceDriver,
                    &json!("primary"),
                )
                .unwrap_err()
                .status,
            404
        );
        assert_eq!(
            fixture.state.session_control(),
            crate::trace::SessionControl {
                settings: Vec::new(),
                secrets: vec![ALT_KEY.to_string()],
                agent_settings: vec!["inference.driver"],
            }
        );

        let (config, inference, choices) = switchable(true);
        let config = ControlConfig {
            secrets: Vec::new(),
            ..config
        };
        let agent_only = ControlState::new(
            &config,
            &inference,
            choices,
            Arc::new(InjectedSecrets::new(&[])),
        );
        assert!(!agent_only.has_controller_surface());
    }

    /// Every settings change after launch starts from the primary: a new state is the manifest.
    #[test]
    fn driver_choice_launch_value_is_primary() {
        let (config, inference, choices) = switchable(false);
        let state = ControlState::new(
            &config,
            &inference,
            choices,
            Arc::new(InjectedSecrets::new(&config.secrets)),
        );
        assert_eq!(
            state.selected(ControllableSetting::InferenceDriver),
            Some(SettingValue::Driver(0))
        );
    }
}
