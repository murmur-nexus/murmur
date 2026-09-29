//! Who may call the A2A door: the tokens a capsule declaring `network.authentication` mints at
//! launch, the check every gated request passes, and the refusals it answers with.
//!
//! **A token is a pure bearer token.** It is a [`crate::mac_token`] token of the family
//! [`DOOR_TOKEN_TAG`], sealed with a [`MintKey`] generated in memory for one session and bound to
//! nothing, so holding it is the whole of what a caller must do. That is what lets a standard A2A
//! client given a token call the door. Its payload is `{"credential": "<name>", "scopes": [...]}`.
//! The key is never written anywhere, so a token dies with the session that minted it and a
//! restart mints new ones. There is no revocation list: dropping the key revokes every token.
//!
//! **The operator token always exists.** It holds every scope in [`DOOR_SCOPES`]. Each credential
//! the manifest declares mints one more token, carrying exactly that credential's scopes, so a
//! declared credential exists to hand out less than the operator token.
//!
//! **Verification order is fixed: header → MAC → payload.** A refusal never says why a token was
//! not accepted, so the door cannot be used as an oracle.
//!
//! **A scope limits methods, not tasks.** Every authenticated caller shares the session's one task
//! and context space, as every caller shares it on a public door.
//!
//! A token reaches no guest, no trace, no log, no error body and no `Debug` output. [`DoorToken`]
//! implements neither `Display` nor `Serialize`, and [`DoorToken::expose`] is the one way to read
//! it.

use murmur_artifact::{
    security_warning_link, NetworkAuthentication, RuntimeManifest, DOOR_SCOPES,
    OPERATOR_CREDENTIAL, W_SEC_032,
};
use serde_json::{json, Value};

use crate::mac_token::{self, MintKey};
use crate::resource_plane::ResourceResponse;

/// The version tag of a door token.
pub const DOOR_TOKEN_TAG: &str = "mdt1";

/// The MAC domain of a door token. Distinct from every other family's, so no other token this
/// process mints can verify at the door.
pub const DOOR_TOKEN_DOMAIN: &[u8] = b"murmur-door-token-v1";

/// The environment variable `mur cancel` and `mur watch` read a door token from when the capsule is
/// named by `--url` rather than by session address.
pub const DOOR_TOKEN_ENV: &str = "MURMUR_DOOR_TOKEN";

/// The scope that reaches the operator resource plane under `/resources/files`.
pub const RESOURCES_FILES_SCOPE: &str = "resources/files";

/// The scheme name in `Authorization`, matched without regard to case.
const BEARER_SCHEME: &str = "bearer";

// ── Tokens ────────────────────────────────────────────────────────────────────

/// One door token's text.
///
/// `Debug` prints `DoorToken(<redacted>)`. There is no `Display` and no `Serialize`, so the text
/// reaches a log line, a trace event or an error body by no route but [`Self::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct DoorToken(String);

impl DoorToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token text, for an `Authorization` header or the line `mur run` prints.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DoorToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DoorToken(<redacted>)")
    }
}

/// The `Authorization` header value that presents `token`.
pub fn bearer_header(token: &DoorToken) -> String {
    format!("Bearer {}", token.expose())
}

// ── Minting and verifying ─────────────────────────────────────────────────────

/// One session's door key and the tokens minted with it.
pub struct DoorAuth {
    key: MintKey,
    /// Operator first, then the declared credentials in name order.
    tokens: Vec<(String, DoorToken)>,
}

impl std::fmt::Debug for DoorAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoorAuth")
            .field(
                "credentials",
                &self
                    .tokens
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl DoorAuth {
    /// Generates this session's key and mints the operator token, holding every scope in
    /// [`DOOR_SCOPES`], and one token per declared credential with exactly its scopes.
    ///
    /// The `Err` is the operating system refusing randomness, and carries no key material.
    pub fn mint(authentication: &NetworkAuthentication) -> Result<Self, String> {
        let key = MintKey::generate()?;
        let operator_scopes: Vec<String> = DOOR_SCOPES.iter().map(|s| s.to_string()).collect();
        let mut tokens = vec![(
            OPERATOR_CREDENTIAL.to_string(),
            seal(&key, OPERATOR_CREDENTIAL, &operator_scopes),
        )];
        for credential in &authentication.credentials {
            tokens.push((
                credential.name.clone(),
                seal(&key, &credential.name, &credential.scopes),
            ));
        }
        Ok(Self { key, tokens })
    }

    /// The token holding every scope.
    pub fn operator_token(&self) -> &DoorToken {
        &self.tokens[0].1
    }

    /// Every token this session minted, operator first, then the declared credentials by name.
    pub fn tokens(&self) -> &[(String, DoorToken)] {
        &self.tokens
    }

    /// The grant `authorization_headers` present: every `Authorization` header value on the
    /// request, as sent.
    ///
    /// No header is [`AuthRefusal::Missing`]. More than one, a scheme other than `Bearer`, a token
    /// this key did not seal, and a payload naming a scope outside [`DOOR_SCOPES`] are all
    /// [`AuthRefusal::Invalid`], indistinguishably.
    pub fn verify(&self, authorization_headers: &[&str]) -> Result<DoorGrant, AuthRefusal> {
        let header = match authorization_headers {
            [] => return Err(AuthRefusal::Missing),
            [header] => header.trim(),
            _ => return Err(AuthRefusal::Invalid),
        };
        let (scheme, token) = header
            .split_once(|c: char| c.is_ascii_whitespace())
            .ok_or(AuthRefusal::Invalid)?;
        if !scheme.eq_ignore_ascii_case(BEARER_SCHEME) {
            return Err(AuthRefusal::Invalid);
        }
        let payload = mac_token::open(
            &self.key,
            DOOR_TOKEN_TAG,
            DOOR_TOKEN_DOMAIN,
            token.trim(),
            &[],
        )
        .map_err(|_| AuthRefusal::Invalid)?;
        let payload: Value = serde_json::from_slice(&payload).map_err(|_| AuthRefusal::Invalid)?;
        let credential = payload["credential"]
            .as_str()
            .ok_or(AuthRefusal::Invalid)?
            .to_string();
        let scopes = payload["scopes"]
            .as_array()
            .ok_or(AuthRefusal::Invalid)?
            .iter()
            .map(|scope| {
                scope
                    .as_str()
                    .filter(|scope| DOOR_SCOPES.contains(scope))
                    .map(str::to_string)
                    .ok_or(AuthRefusal::Invalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DoorGrant { credential, scopes })
    }
}

fn seal(key: &MintKey, credential: &str, scopes: &[String]) -> DoorToken {
    let payload = json!({"credential": credential, "scopes": scopes});
    DoorToken(mac_token::seal(
        key,
        DOOR_TOKEN_TAG,
        DOOR_TOKEN_DOMAIN,
        payload.to_string().as_bytes(),
        &[],
    ))
}

/// What a verified token reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoorGrant {
    credential: String,
    scopes: Vec<String>,
}

impl DoorGrant {
    /// The credential the token was minted for: `operator` or a declared name.
    pub fn credential(&self) -> &str {
        &self.credential
    }

    /// Whether the token carries `scope`.
    pub fn allows(&self, scope: &str) -> bool {
        self.scopes.iter().any(|held| held == scope)
    }

    /// `Ok` when the token carries `scope`, and the `403` refusal naming both otherwise.
    pub fn require(&self, scope: &str) -> Result<(), AuthRefusal> {
        if self.allows(scope) {
            Ok(())
        } else {
            Err(AuthRefusal::InsufficientScope {
                credential: self.credential.clone(),
                scope: scope.to_string(),
            })
        }
    }
}

/// Why the door refused a request before looking at what it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRefusal {
    /// No `Authorization` header.
    Missing,
    /// An `Authorization` header that does not present a token this session minted, for any
    /// reason at all.
    Invalid,
    /// A valid token whose credential does not carry the scope the request needs.
    InsufficientScope { credential: String, scope: String },
}

impl AuthRefusal {
    pub fn status(&self) -> u16 {
        match self {
            AuthRefusal::Missing | AuthRefusal::Invalid => 401,
            AuthRefusal::InsufficientScope { .. } => 403,
        }
    }

    /// The body's `error` code.
    pub fn code(&self) -> &'static str {
        match self {
            AuthRefusal::Missing => "unauthenticated",
            AuthRefusal::Invalid => "invalid_token",
            AuthRefusal::InsufficientScope { .. } => "insufficient_scope",
        }
    }

    /// The `www-authenticate` challenge, in RFC 6750's terms, for a door whose realm is `realm`.
    pub fn challenge(&self, realm: &str) -> String {
        match self {
            AuthRefusal::Missing => format!("Bearer realm=\"{realm}\""),
            AuthRefusal::Invalid => format!("Bearer realm=\"{realm}\", error=\"invalid_token\""),
            AuthRefusal::InsufficientScope { scope, .. } => {
                format!("Bearer realm=\"{realm}\", error=\"insufficient_scope\", scope=\"{scope}\"")
            }
        }
    }

    /// The body's `message`. The `403` names the credential and the scope it lacks, and goes only
    /// to that credential's holder; neither `401` says why.
    pub fn message(&self) -> String {
        match self {
            AuthRefusal::Missing => "this door takes a token this capsule's runtime minted, as \
                                     Authorization: Bearer <token>"
                .to_string(),
            AuthRefusal::Invalid => {
                "the Authorization header does not present a token this session minted".to_string()
            }
            AuthRefusal::InsufficientScope { credential, scope } => {
                format!("credential '{credential}' does not reach {scope}")
            }
        }
    }

    /// The refusal as an HTTP response: status, challenge, and the
    /// `{"error": <code>, "message": <sentence>}` body the resource plane also answers with.
    pub fn response(&self, realm: &str) -> ResourceResponse {
        let body = json!({"error": self.code(), "message": self.message()});
        ResourceResponse::framed(
            self.status(),
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("www-authenticate".to_string(), self.challenge(realm)),
            ],
            serde_json::to_vec(&body).unwrap_or_default(),
        )
    }
}

// ── The public-door warning ───────────────────────────────────────────────────

/// Whether a `--bind` address keeps the door on this host: an IPv4 address in `127.0.0.0/8`,
/// `::1`, or `localhost`. Every other address, `0.0.0.0` and `::` among them, and every other
/// hostname, exposes it.
pub fn is_loopback_bind(addr: &str) -> bool {
    let addr = addr.trim();
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = addr
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(addr);
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => v6 == std::net::Ipv6Addr::LOCALHOST,
        Err(_) => false,
    }
}

/// How the door is reachable off this host.
#[derive(Debug, Clone, Copy)]
pub enum DoorExposure<'a> {
    /// `mur run --bind <addr>` with a non-loopback address.
    Bound(&'a str),
    /// `mur deploy`, which publishes the loopback-bound port at this URL.
    Published(&'a str),
}

impl DoorExposure<'_> {
    fn phrase(&self) -> String {
        match self {
            DoorExposure::Bound(addr) => format!("bound to {addr}"),
            DoorExposure::Published(url) => format!("published at {url}"),
        }
    }
}

/// What a public door hands any caller, read from the manifest alone so that every surface
/// renders the same line for the same manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicDoorDisclosure {
    pub methods: Vec<&'static str>,
    pub tools: Vec<String>,
    pub shell: bool,
    pub network: bool,
    pub planes: Vec<&'static str>,
}

impl PublicDoorDisclosure {
    /// The disclosure of `manifest`'s door: the methods served under its
    /// `lifecycle.task_acceptance`, its LLM-visible artifacts, whether it grants shell and network
    /// egress, and its declared planes.
    pub fn of(manifest: &RuntimeManifest) -> Self {
        let acceptance = manifest
            .lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.task_acceptance.clone())
            .unwrap_or_default();
        let capabilities = manifest.capabilities.as_ref();
        let exports = manifest.exports.as_ref();
        let planes = [
            (exports.is_some_and(|e| e.files.is_some()), "files"),
            (
                exports.is_some_and(|e| e.peer_files.is_some()),
                "peer_files",
            ),
        ]
        .into_iter()
        .filter_map(|(declared, name)| declared.then_some(name))
        .collect();
        Self {
            methods: crate::identity::served_methods(&acceptance, false),
            tools: manifest
                .artifacts
                .iter()
                .filter(|artifact| artifact.runtime.is_llm_visible())
                .map(|artifact| artifact.name.clone())
                .collect(),
            shell: capabilities
                .and_then(|c| c.shell.as_ref())
                .is_some_and(|shell| !shell.allow.is_empty()),
            network: capabilities
                .and_then(|c| c.network.as_ref())
                .is_some_and(|network| !network.allow.is_empty()),
            planes,
        }
    }
}

/// The one `W-SEC-032` line.
pub fn render_public_door_warning(
    exposure: DoorExposure<'_>,
    disclosure: &PublicDoorDisclosure,
) -> String {
    format!(
        "warning[{W_SEC_032}]: the door is {} and network.authentication is not declared — any \
         caller that reaches it can call {}, and the agent card publishes to any A2A client the \
         session id, tools [{}], shell: {}, network: {} and planes [{}] ({})",
        exposure.phrase(),
        disclosure.methods.join(", "),
        disclosure.tools.join(", "),
        disclosure.shell,
        disclosure.network,
        disclosure.planes.join(", "),
        security_warning_link(W_SEC_032),
    )
}

/// The `W-SEC-032` line for `manifest` exposed as `exposure`, or `None` when the manifest declares
/// `network.authentication` or serves no door at all (no `inference:` block).
pub fn public_door_warning(
    manifest: &RuntimeManifest,
    exposure: DoorExposure<'_>,
) -> Option<String> {
    let authenticated = manifest
        .network
        .as_ref()
        .is_some_and(|network| network.authentication.is_some());
    if authenticated || manifest.inference.is_none() {
        return None;
    }
    Some(render_public_door_warning(
        exposure,
        &PublicDoorDisclosure::of(manifest),
    ))
}

/// The `W-SEC-032` line for `mur run --bind <bind_addr>` and `mur doctor --bind <bind_addr>`, or
/// `None` for a loopback bind.
pub fn public_door_bind_warning(manifest: &RuntimeManifest, bind_addr: &str) -> Option<String> {
    if is_loopback_bind(bind_addr) {
        return None;
    }
    public_door_warning(manifest, DoorExposure::Bound(bind_addr))
}

/// The one-line posture `mur doctor` prints for the door: `public (no network.authentication)`,
/// or `bearer — operator, <name> (<scopes>), …`.
pub fn door_posture(manifest: &RuntimeManifest) -> String {
    match manifest
        .network
        .as_ref()
        .and_then(|network| network.authentication.as_ref())
    {
        None => "door: public (no network.authentication)".to_string(),
        Some(authentication) => {
            let mut credentials = vec![OPERATOR_CREDENTIAL.to_string()];
            credentials.extend(authentication.credentials.iter().map(|credential| {
                format!("{} ({})", credential.name, credential.scopes.join(", "))
            }));
            format!("door: bearer — {}", credentials.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{AuthenticationScheme, DoorCredential};

    fn authentication() -> NetworkAuthentication {
        NetworkAuthentication {
            scheme: AuthenticationScheme::Bearer,
            credentials: vec![
                DoorCredential {
                    name: "reader".to_string(),
                    scopes: vec!["resources/files".to_string()],
                },
                DoorCredential {
                    name: "watcher".to_string(),
                    scopes: vec!["tasks/get".to_string(), "stream/watch".to_string()],
                },
            ],
        }
    }

    fn header(token: &DoorToken) -> String {
        bearer_header(token)
    }

    #[test]
    fn door_auth_mints_operator_first_then_declared_credentials_by_name() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let names: Vec<&str> = auth.tokens().iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["operator", "reader", "watcher"]);
        assert_eq!(auth.operator_token(), &auth.tokens()[0].1);
        for (_, token) in auth.tokens() {
            assert!(token.expose().starts_with("mdt1."), "{}", token.expose());
        }
    }

    #[test]
    fn door_auth_token_payload_is_credential_and_scopes() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let payload = |token: &DoorToken| -> Value {
            serde_json::from_slice(
                &mac_token::payload_segment(DOOR_TOKEN_TAG, token.expose()).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            payload(auth.operator_token()),
            json!({"credential": "operator", "scopes": DOOR_SCOPES})
        );
        assert_eq!(
            payload(&auth.tokens()[2].1),
            json!({"credential": "watcher", "scopes": ["tasks/get", "stream/watch"]})
        );
        let raw = mac_token::payload_segment(DOOR_TOKEN_TAG, auth.tokens()[1].1.expose()).unwrap();
        assert_eq!(
            String::from_utf8(raw).unwrap(),
            r#"{"credential":"reader","scopes":["resources/files"]}"#
        );
    }

    #[test]
    fn door_auth_tokens_open_under_the_door_family_with_nothing_bound() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let token = auth.operator_token().expose();
        assert!(mac_token::open(&auth.key, DOOR_TOKEN_TAG, DOOR_TOKEN_DOMAIN, token, &[]).is_ok());
        assert!(mac_token::open(
            &auth.key,
            DOOR_TOKEN_TAG,
            b"murmur-control-token-v1",
            token,
            &[]
        )
        .is_err());
    }

    #[test]
    fn door_auth_verify_grants_each_token_its_scopes() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let operator = auth.verify(&[&header(auth.operator_token())]).unwrap();
        assert_eq!(operator.credential(), "operator");
        for scope in DOOR_SCOPES {
            assert!(operator.allows(scope), "{scope}");
        }
        let watcher = auth.verify(&[&header(&auth.tokens()[2].1)]).unwrap();
        assert_eq!(watcher.credential(), "watcher");
        assert!(watcher.allows("tasks/get"));
        assert!(watcher.allows("stream/watch"));
        assert!(!watcher.allows("message/send"));
        assert_eq!(
            watcher.require("message/send"),
            Err(AuthRefusal::InsufficientScope {
                credential: "watcher".to_string(),
                scope: "message/send".to_string(),
            })
        );
        assert_eq!(watcher.require("tasks/get"), Ok(()));
    }

    #[test]
    fn door_auth_verify_refusals() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let other = DoorAuth::mint(&authentication()).unwrap();
        let token = auth.operator_token().expose().to_string();

        assert_eq!(auth.verify(&[]), Err(AuthRefusal::Missing));
        let good = format!("Bearer {token}");
        assert_eq!(auth.verify(&[&good, &good]), Err(AuthRefusal::Invalid));
        for header in [
            format!("Basic {token}"),
            format!("Token {token}"),
            token.clone(),
            "Bearer".to_string(),
            "Bearer ".to_string(),
            "Bearer garbage".to_string(),
            format!("Bearer {}", other.operator_token().expose()),
            format!("Bearer {}", token.to_uppercase()),
            format!("Bearer {}x", token),
        ] {
            assert_eq!(
                auth.verify(&[&header]),
                Err(AuthRefusal::Invalid),
                "{header}"
            );
        }
        // The scheme is matched without regard to case; the token verbatim.
        for scheme in ["bearer", "BEARER", "BeArEr"] {
            assert!(
                auth.verify(&[&format!("{scheme} {token}")]).is_ok(),
                "{scheme}"
            );
        }
    }

    #[test]
    fn door_auth_verify_refuses_a_sealed_payload_with_a_scope_outside_the_set() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let forged = seal(&auth.key, "watcher", &["tasks/list".to_string()]);
        assert_eq!(auth.verify(&[&header(&forged)]), Err(AuthRefusal::Invalid));
        let not_json = mac_token::seal(&auth.key, DOOR_TOKEN_TAG, DOOR_TOKEN_DOMAIN, b"[]", &[]);
        assert_eq!(
            auth.verify(&[&format!("Bearer {not_json}")]),
            Err(AuthRefusal::Invalid)
        );
    }

    #[test]
    fn door_auth_debug_prints_no_token() {
        let auth = DoorAuth::mint(&authentication()).unwrap();
        let token = auth.operator_token().clone();
        assert_eq!(format!("{token:?}"), "DoorToken(<redacted>)");
        let debug = format!("{auth:?}");
        for (_, token) in auth.tokens() {
            assert!(!debug.contains(token.expose()), "{debug}");
        }
        assert!(debug.contains("watcher"), "{debug}");
    }

    #[test]
    fn door_auth_refusal_responses() {
        let realm = "my-agent";
        let cases = [
            (
                AuthRefusal::Missing,
                401,
                "Bearer realm=\"my-agent\"",
                "unauthenticated",
            ),
            (
                AuthRefusal::Invalid,
                401,
                "Bearer realm=\"my-agent\", error=\"invalid_token\"",
                "invalid_token",
            ),
            (
                AuthRefusal::InsufficientScope {
                    credential: "watcher".to_string(),
                    scope: "message/send".to_string(),
                },
                403,
                "Bearer realm=\"my-agent\", error=\"insufficient_scope\", scope=\"message/send\"",
                "insufficient_scope",
            ),
        ];
        for (refusal, status, challenge, code) in cases {
            let response = refusal.response(realm);
            assert_eq!(response.status, status);
            assert!(response
                .headers
                .contains(&("www-authenticate".to_string(), challenge.to_string())));
            let body: Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(body["error"], code);
            assert!(body["message"].is_string());
        }
        let body: Value = serde_json::from_slice(
            &AuthRefusal::InsufficientScope {
                credential: "watcher".to_string(),
                scope: "message/send".to_string(),
            }
            .response(realm)
            .body,
        )
        .unwrap();
        assert_eq!(
            body["message"],
            "credential 'watcher' does not reach message/send"
        );
    }

    /// The manifest's scope list and the door's method table cannot drift: every gated method is a
    /// scope, and every scope but the resource plane's is a gated method.
    #[test]
    fn door_auth_scopes_are_the_gated_methods_and_the_resource_plane() {
        use crate::identity::DoorMethod;
        let mut from_methods: Vec<&str> = DoorMethod::ALL
            .into_iter()
            .filter(|method| *method != DoorMethod::GetAuthenticatedExtendedCard)
            .map(DoorMethod::wire_name)
            .collect();
        from_methods.push(RESOURCES_FILES_SCOPE);
        for scope in &from_methods {
            assert!(DOOR_SCOPES.contains(scope), "{scope} is not in DOOR_SCOPES");
        }
        for scope in DOOR_SCOPES {
            assert!(
                from_methods.contains(scope),
                "{scope} names no gated method or plane"
            );
        }
        assert_eq!(from_methods.len(), DOOR_SCOPES.len());
        for method in DoorMethod::ALL {
            assert_eq!(
                method.scope(),
                (method != DoorMethod::GetAuthenticatedExtendedCard).then(|| method.wire_name())
            );
        }
    }

    #[test]
    fn public_door_warning_is_loopback_bind() {
        for addr in [
            "127.0.0.1",
            "127.1.2.3",
            "127.255.255.255",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
        ] {
            assert!(is_loopback_bind(addr), "{addr}");
        }
        for addr in [
            "0.0.0.0",
            "::",
            "[::]",
            "10.0.0.1",
            "192.168.1.2",
            "128.0.0.1",
            "example.com",
            "localhost.localdomain",
            "::2",
            "",
        ] {
            assert!(!is_loopback_bind(addr), "{addr}");
        }
    }

    fn manifest(extra: &str) -> RuntimeManifest {
        RuntimeManifest::from_yaml_str(&format!(
            "name: my-agent\nversion: 0.1.0\nartifacts:\n  - name: bash\n    version: 1.0.0\n    \
             runtime: tool\n  - name: drv\n    version: 1.0.0\n    runtime: driver\n    gateway:\n      \
             endpoint: https://api.example.com\n      api_key: k\n\
             inference:\n  transport: http\n  model: m\n  driver:\n    artifact: drv\n\
             capabilities:\n  shell:\n    allow: [git]\n  network:\n    allow: [api.example.com]\n\
             exports:\n  files:\n    root: out\n    mode: read-only\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn public_door_warning_text() {
        let line = public_door_bind_warning(&manifest(""), "0.0.0.0").expect("a warning");
        assert_eq!(
            line,
            format!(
                "warning[W-SEC-032]: the door is bound to 0.0.0.0 and network.authentication is \
                 not declared — any caller that reaches it can call message/send, message/stream, \
                 stream/watch, tasks/get, tasks/cancel, session/stop, and the agent card publishes \
                 to any A2A client the session id, tools [bash], shell: true, network: true and \
                 planes [files] ({})",
                security_warning_link(W_SEC_032)
            )
        );
        let published = public_door_warning(
            &manifest("lifecycle:\n  task_acceptance: none\n"),
            DoorExposure::Published("http://203.0.113.9:41873"),
        )
        .unwrap();
        assert!(
            published.contains("published at http://203.0.113.9:41873"),
            "{published}"
        );
        assert!(
            published.contains("can call stream/watch, tasks/get, tasks/cancel, session/stop,"),
            "{published}"
        );
    }

    #[test]
    fn public_door_warning_is_silent_for_loopback_and_authenticated_doors() {
        assert_eq!(public_door_bind_warning(&manifest(""), "127.0.0.1"), None);
        assert_eq!(public_door_bind_warning(&manifest(""), "localhost"), None);
        let authenticated = manifest("network:\n  authentication:\n    scheme: bearer\n");
        assert_eq!(public_door_bind_warning(&authenticated, "0.0.0.0"), None);
        assert_eq!(
            public_door_warning(&authenticated, DoorExposure::Published("http://h:1")),
            None
        );
    }

    #[test]
    fn public_door_warning_posture_line() {
        assert_eq!(
            door_posture(&manifest("")),
            "door: public (no network.authentication)"
        );
        let authenticated = manifest(
            "network:\n  authentication:\n    scheme: bearer\n    credentials:\n      \
             watcher: {scopes: [tasks/get, stream/watch]}\n      reader: {scopes: [resources/files]}\n",
        );
        assert_eq!(
            door_posture(&authenticated),
            "door: bearer — operator, reader (resources/files), watcher (tasks/get, stream/watch)"
        );
    }
}
