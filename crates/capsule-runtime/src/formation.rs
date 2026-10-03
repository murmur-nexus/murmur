//! The formation a session belongs to, and the one id that names it.
//!
//! A formation is the set of sessions launched together to carry out one task. Its id is minted
//! once, by whatever launches the formation's members, and handed to each member through
//! [`FORMATION_ID_ENV`]. A member's own delegation plane hands the same id to every child it
//! spawns, so a child joins its parent's formation and never mints one. Nothing else mints: a
//! standalone `mur run` that delegates grows a delegation tree, not a formation, and that tree's
//! sessions carry no formation id — they are joined by lineage (`session_start.spawned_by`,
//! `delegation_id`) instead.
//!
//! **The id groups; it grants nothing.** No authorization decision reads it alone — not the door's
//! Bearer check, not task-origin classification, not peer handoff, not the spawn referee. What a
//! member may call is decided by a formation token ([`crate::formation_credentials`]), which the
//! launcher signs over the id together with the caller and the callee; a session that merely
//! carries the id, a delegated child among them, holds no such token and reaches nobody by it.
//!
//! **It belongs to the session, not the process.** [`FormationId::from_env`] is the one read of
//! [`FORMATION_ID_ENV`] from a process environment, and only `mur run` — whose process is one
//! session — calls it, before the workspace `.env` is loaded. From there the id travels on
//! [`crate::StageRequest::formation_id`] and the staged session, which is what hooks and the
//! delegation plane read, so an in-process caller staging two sessions gives each its own.
//!
//! **The shape is strict.** `frm_` followed by the 32 lowercase hex digits of a UUIDv7: opaque,
//! derived from nothing about the formation, its members or the host, and time-ordered so ids sort
//! by launch. A value that sessions are grouped on admits no near-misses, so anything else in the
//! variable — other than nothing at all — refuses the launch.
//!
//! **A member's guests are handed its callees' names.** [`FORMATION_PEERS_ENV`] is set by a
//! member's runtime inside its own WASM guests, never by a launcher in a process environment: each
//! callee at its virtual address `http://<name>.formation.invalid`, which only the runtime's egress
//! can reach. `mur run` refuses a launch whose own environment carries the variable
//! ([`refuse_formation_peers_in_process_env`]). A delegated child and a native process are never
//! handed it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::errors::RuntimeError;

/// The variable a launcher sets in a member's environment, and a member's runtime sets in each
/// child's, carrying its [`FormationId`].
pub const FORMATION_ID_ENV: &str = "MURMUR_FORMATION_ID";

/// Prefix on every formation id. Distinct from `ses_`/`ctx_`/`tsk_`/`dlg_`/`wrk_`, so a reader can
/// tell at a glance which id space a value belongs to.
pub const FORMATION_ID_PREFIX: &str = "frm_";

/// How many hex digits follow [`FORMATION_ID_PREFIX`]: a UUID in its simple form.
const FORMATION_ID_DIGITS: usize = 32;

/// The id of the formation a session belongs to, matching `^frm_[0-9a-f]{32}$`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FormationId(String);

impl FormationId {
    /// A fresh formation id, for a launcher starting a formation's members.
    ///
    /// Two launches of the same formation get two different ids.
    pub fn mint() -> Self {
        Self(format!(
            "{FORMATION_ID_PREFIX}{}",
            uuid::Uuid::now_v7().simple()
        ))
    }

    /// Accept `value` only if it is exactly a formation id; nothing is trimmed.
    ///
    /// The error's `reason` names what is wrong with the value and never contains it.
    pub fn parse(value: &str) -> Result<Self, RuntimeError> {
        let unreadable = |reason: &str| RuntimeError::FormationIdUnreadable {
            reason: reason.to_string(),
        };
        let Some(digits) = value.strip_prefix(FORMATION_ID_PREFIX) else {
            return Err(unreadable("it does not start with 'frm_'"));
        };
        if digits.len() != FORMATION_ID_DIGITS {
            return Err(unreadable(
                "'frm_' must be followed by exactly 32 hex digits",
            ));
        }
        if !digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(unreadable(
                "it contains a character outside lowercase hex (0-9, a-f)",
            ));
        }
        Ok(Self(value.to_string()))
    }

    /// This session's formation, read from [`FORMATION_ID_ENV`] in the process environment.
    ///
    /// `Ok(None)` for a session nobody placed in a formation — the variable absent, or blank —
    /// which is the ordinary case. Any other value that is not a formation id is an error and not
    /// an absence: a session that cannot tell which formation it belongs to would be grouped with
    /// the wrong one, or with none.
    pub fn from_env() -> Result<Option<Self>, RuntimeError> {
        let Some(raw) = std::env::var_os(FORMATION_ID_ENV) else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy();
        if raw.trim().is_empty() {
            return Ok(None);
        }
        Self::parse(raw.trim()).map(Some)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// When this id was minted, in milliseconds since the Unix epoch: the first 12 hex digits of
    /// its UUIDv7, read the same way `retention::session_id_timestamp_ms` reads a `ses_` id.
    ///
    /// A member's session is minted after the formation it joins, so a reader looking for members
    /// can pass over every session older than this.
    pub fn minted_at_ms(&self) -> u64 {
        // `parse` and `mint` admit only 32 lowercase hex digits, so this always reads.
        crate::retention::uuid_v7_timestamp_ms(&self.0, FORMATION_ID_PREFIX).unwrap_or(0)
    }

    /// The `(name, value)` pair a launcher writes into a member's or a child's environment.
    pub fn env_pair(&self) -> (&'static str, String) {
        (FORMATION_ID_ENV, self.0.clone())
    }
}

impl fmt::Display for FormationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A bare string, `"frm_…"`.
impl Serialize for FormationId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Only through [`FormationId::parse`], so a file carrying anything but a formation id does not
/// deserialize at all. The error names what is wrong and never contains the value.
impl<'de> Deserialize<'de> for FormationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|error| match error {
            RuntimeError::FormationIdUnreadable { reason } => {
                serde::de::Error::custom(format!("not a formation id: {reason}"))
            }
            _ => serde::de::Error::custom("not a formation id"),
        })
    }
}

/// The variable a formation member's runtime sets in each of its WASM guests, naming every member
/// the roster lets it call at that member's virtual address.
///
/// Runtime-owned: no declaration supplies or displaces it, no native process or delegated child is
/// handed it, and `mur run` refuses a process environment that carries it.
pub const FORMATION_PEERS_ENV: &str = "MURMUR_FORMATION_PEERS";

/// The only scheme a peer address may carry. Members of one formation share a host.
const PEER_URL_SCHEME: &str = "http://";

/// One member a member may call, and the address its guests call it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormationPeer {
    /// The member's roster name, matching `^[a-z][a-z0-9-]{0,31}$`.
    pub name: String,
    /// `http://host` or `http://host:port`, with no path: the virtual address a guest is handed,
    /// or the real door URL the channel carries.
    pub url: String,
}

/// The value of [`FORMATION_PEERS_ENV`]: `name=url` pairs separated by single spaces, in roster
/// order, with at least one pair.
///
/// Space-separated rather than JSON so a shell guest reads it with
/// `for p in $MURMUR_FORMATION_PEERS` and no parser. Unambiguous, because neither a member name
/// nor an `http://host:port` URL can contain a space or an `=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormationPeers(Vec<FormationPeer>);

impl FormationPeers {
    /// The peers in the order given: one or more, each a member name at most once, each at an
    /// `http://host[:port]` URL with no path.
    pub fn new(peers: Vec<FormationPeer>) -> Result<Self, RuntimeError> {
        if peers.is_empty() {
            return Err(peers_unreadable(
                "it names no peer; a member that may call nobody is handed no variable at all",
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for (index, peer) in peers.iter().enumerate() {
            let position = index + 1;
            if let Some(problem) = murmur_artifact::member_name_format_error(&peer.name) {
                return Err(peers_unreadable(&format!(
                    "pair {position} names a member whose name {problem}"
                )));
            }
            if !seen.insert(peer.name.as_str()) {
                return Err(peers_unreadable(&format!(
                    "pair {position} names '{}' a second time",
                    peer.name
                )));
            }
            if let Some(problem) = url_format_error(&peer.url, false) {
                return Err(peers_unreadable(&format!(
                    "pair {position} ('{}') {problem}",
                    peer.name
                )));
            }
        }
        Ok(Self(peers))
    }

    /// The variable's value: `name=url` pairs joined by single spaces, in order.
    pub fn render(&self) -> String {
        self.0
            .iter()
            .map(|peer| format!("{}={}", peer.name, peer.url))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The `(name, value)` pair a member's runtime writes into each of its WASM guests'
    /// environments.
    pub fn env_pair(&self) -> (&'static str, String) {
        (FORMATION_PEERS_ENV, self.render())
    }

    pub fn peers(&self) -> &[FormationPeer] {
        &self.0
    }
}

/// Whether `name` is one of the variables that carry what a formation member was handed —
/// [`FORMATION_PEERS_ENV`] or [`crate::formation_credentials::FORMATION_CHANNEL_ENV`]. Neither is
/// ever resolved from the host for a native process or handed to a delegated child.
pub(crate) fn is_member_grant_env(name: &str) -> bool {
    name == FORMATION_PEERS_ENV || name == crate::formation_credentials::FORMATION_CHANNEL_ENV
}

/// `Err` when [`FORMATION_PEERS_ENV`] is set in this process's environment.
///
/// Only a member's runtime sets the variable, and only inside its own guests, from what the
/// formation channel delivered. A value in `mur run`'s own environment came from somewhere else —
/// a shell, a parent that should not have passed it on — and is refused rather than ignored, so
/// nobody is led to believe it was read. The value is never read or quoted.
pub fn refuse_formation_peers_in_process_env() -> Result<(), RuntimeError> {
    if std::env::var_os(FORMATION_PEERS_ENV).is_some() {
        return Err(peers_unreadable("it is set in this process's environment"));
    }
    Ok(())
}

fn peers_unreadable(reason: &str) -> RuntimeError {
    RuntimeError::FormationPeersUnreadable {
        reason: reason.to_string(),
    }
}

/// Why `url` is not `http://host:port`, or `None` when it is: the shape of a real door address.
pub(crate) fn peer_url_format_error(url: &str) -> Option<String> {
    url_format_error(url, true)
}

/// Why `url` is not `http://host[:port]`, or `None` when it is. The port is optional only when
/// `port_required` is false.
fn url_format_error(url: &str, port_required: bool) -> Option<String> {
    let Some(authority) = url.strip_prefix(PEER_URL_SCHEME) else {
        return Some("has a URL that does not start with 'http://'".to_string());
    };
    // A bracketed IPv6 host ends in `]` when no port follows it.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !authority.ends_with(']') => (host, Some(port)),
        _ => (authority, None),
    };
    if port.is_none() && port_required {
        return Some("has a URL with no port".to_string());
    }
    let bracketed_ip = host.len() > 2 && host.starts_with('[') && host.ends_with(']');
    let plain_host = !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if !(bracketed_ip || plain_host) {
        return Some("has a URL whose host is not a hostname or an IP address".to_string());
    }
    let port = port?;
    let digits_only = !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit());
    match port.parse::<u16>() {
        Ok(port) if digits_only && port > 0 => None,
        _ => Some("has a URL whose port is not a number from 1 to 65535".to_string()),
    }
}

/// Serializes the unit tests that set or remove [`FORMATION_ID_ENV`] or [`FORMATION_PEERS_ENV`] in
/// this test binary's process environment, which every test thread shares.
#[cfg(test)]
pub(crate) static FORMATION_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refusal(value: &str) -> String {
        match FormationId::parse(value) {
            Err(error @ RuntimeError::FormationIdUnreadable { .. }) => error.to_string(),
            other => panic!("expected FormationIdUnreadable, got {other:?}"),
        }
    }

    #[test]
    fn a_minted_formation_id_is_well_formed() {
        let id = FormationId::mint();
        assert_eq!(id.as_str().len(), 36);
        assert!(id.as_str().starts_with(FORMATION_ID_PREFIX));
        assert_eq!(FormationId::parse(id.as_str()).unwrap(), id);
        assert_eq!(id.to_string(), id.as_str());
        assert_eq!(id.env_pair(), (FORMATION_ID_ENV, id.as_str().to_string()));
    }

    #[test]
    fn minted_formation_ids_are_distinct_and_sort_in_mint_order() {
        let minted: Vec<FormationId> = (0..1_000).map(|_| FormationId::mint()).collect();
        let distinct: std::collections::HashSet<&FormationId> = minted.iter().collect();
        assert_eq!(distinct.len(), minted.len());
        let mut sorted: Vec<&str> = minted.iter().map(FormationId::as_str).collect();
        sorted.sort_unstable();
        let in_mint_order: Vec<&str> = minted.iter().map(FormationId::as_str).collect();
        assert_eq!(sorted, in_mint_order);
    }

    #[test]
    fn a_formation_id_parse_refuses_near_misses_without_echoing_them() {
        let minted = FormationId::mint();
        let digits = &minted.as_str()[FORMATION_ID_PREFIX.len()..];
        let near_misses = [
            "frm_".to_string(),
            format!("frm_{}", &digits[..31]),
            format!("frm_{digits}0"),
            format!("frm_{}", digits.to_uppercase()),
            format!("frm_{}A", &digits[..31]),
            format!("dlg_{digits}"),
            format!("ses_{digits}"),
            format!(" {}", minted.as_str()),
            format!("{} ", minted.as_str()),
            format!("x{}", minted.as_str()),
            format!("{}x", minted.as_str()),
            format!("frm_{}g", &digits[..31]),
            format!("frm_{}-", &digits[..31]),
            "not-a-formation".to_string(),
        ];
        for value in &near_misses {
            let message = refusal(value);
            // The bare prefix is the one input the refusal's own wording legitimately contains.
            if value != FORMATION_ID_PREFIX {
                assert!(
                    !message.contains(value.trim()),
                    "the refusal echoes the value {value:?}: {message}"
                );
            }
            assert!(!message.contains(&digits[..16]), "{message}");
            assert!(!message.contains(&digits[..16].to_uppercase()), "{message}");
            assert!(message.contains("MURMUR_FORMATION_ID"), "{message}");
        }
        assert!(refusal("dlg_x").contains("does not start with 'frm_'"));
        assert!(refusal(&format!("frm_{}", &digits[..31])).contains("exactly 32 hex digits"));
        assert!(refusal(&format!("frm_{}", digits.to_uppercase())).contains("lowercase hex"));
        assert!(refusal(&format!("frm_{}g", &digits[..31])).contains("lowercase hex"));
    }

    #[test]
    fn a_formation_id_is_read_from_the_process_environment_only_when_well_formed() {
        let _guard = env_guard();
        std::env::remove_var(FORMATION_ID_ENV);
        assert_eq!(FormationId::from_env().unwrap(), None);

        std::env::set_var(FORMATION_ID_ENV, "   ");
        assert_eq!(FormationId::from_env().unwrap(), None);

        let minted = FormationId::mint();
        std::env::set_var(FORMATION_ID_ENV, minted.as_str());
        assert_eq!(FormationId::from_env().unwrap(), Some(minted.clone()));

        std::env::set_var(FORMATION_ID_ENV, format!(" {} ", minted.as_str()));
        assert_eq!(FormationId::from_env().unwrap(), Some(minted));

        std::env::set_var(FORMATION_ID_ENV, "frm_ABC");
        let result = FormationId::from_env();
        std::env::remove_var(FORMATION_ID_ENV);
        assert!(matches!(
            result,
            Err(RuntimeError::FormationIdUnreadable { .. })
        ));
    }

    #[test]
    fn a_formation_id_serializes_as_a_bare_string_and_round_trips() {
        let id = FormationId::mint();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        let back: FormationId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn a_malformed_formation_id_does_not_deserialize_and_is_not_echoed() {
        let minted = FormationId::mint();
        let digits = &minted.as_str()[FORMATION_ID_PREFIX.len()..];
        for value in [
            "frm_ABC".to_string(),
            format!("frm_{}", digits.to_uppercase()),
            format!("ses_{digits}"),
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let error = serde_json::from_str::<FormationId>(&json)
                .expect_err("a malformed id deserializes")
                .to_string();
            assert!(
                !error.contains(&value),
                "the error echoes {value:?}: {error}"
            );
            assert!(!error.contains(&digits[..16]), "{error}");
            assert!(!error.contains(&digits[..16].to_uppercase()), "{error}");
            assert!(error.contains("not a formation id"), "{error}");
        }
        assert!(serde_json::from_str::<FormationId>("42").is_err());
    }

    #[test]
    fn a_formation_id_was_minted_when_its_uuid_says() {
        let id = FormationId::mint();
        let tail = &id.as_str()[FORMATION_ID_PREFIX.len()..];
        assert_eq!(
            Some(id.minted_at_ms()),
            crate::retention::session_id_timestamp_ms(&format!("ses_{tail}"))
        );
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(id.minted_at_ms() <= now_ms);
        assert!(now_ms - id.minted_at_ms() < 60_000);
        let fixed = FormationId::parse("frm_00000000000a0000000000000000000f").unwrap();
        assert_eq!(fixed.minted_at_ms(), 10);
    }

    /// Every `.rs` file under the named workspace crate's `src`, as `(crate/src/path, contents)`.
    fn sources_of(crate_dir: &str) -> Vec<(String, String)> {
        crate::source_scan::sources_under(&crate::source_scan::sibling_crate_src(crate_dir))
            .into_iter()
            .map(|(path, text)| (format!("{crate_dir}/src/{path}"), text))
            .collect()
    }

    const FORMATION_TOKENS: [&str; 4] = [
        "FormationId",
        "formation_id",
        "FORMATION_ID_ENV",
        "MURMUR_FORMATION_ID",
    ];

    #[test]
    fn no_authorization_path_reads_a_formation_id() {
        let own = sources_of("capsule-runtime");
        let mut guarded = sources_of("mur-roost");
        for file in [
            "door_auth.rs",
            "origin.rs",
            "peer_handoff.rs",
            "spawn_credential.rs",
            "spawn_envelope.rs",
        ] {
            let path = format!("capsule-runtime/src/{file}");
            let source = own
                .iter()
                .find(|(name, _)| *name == path)
                .unwrap_or_else(|| panic!("{path} is gone"));
            guarded.push(source.clone());
        }
        for (path, text) in guarded {
            for token in FORMATION_TOKENS {
                assert!(
                    !text.contains(token),
                    "{path} names {token}; a formation id confers no reachability"
                );
            }
        }
    }

    /// Every `env::var(..)` / `env::var_os(..)` call in `text`, as the text of its argument list.
    fn env_reads(text: &str) -> Vec<&str> {
        let mut calls = Vec::new();
        for opener in ["env::var(", "env::var_os("] {
            let mut rest = text;
            while let Some(at) = rest.find(opener) {
                let args = &rest[at + opener.len()..];
                let mut depth = 1usize;
                let end = args
                    .char_indices()
                    .find_map(|(index, ch)| {
                        match ch {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        (depth == 0).then_some(index)
                    })
                    .unwrap_or(args.len());
                calls.push(&args[..end]);
                rest = &args[end..];
            }
        }
        calls
    }

    #[test]
    fn the_formation_id_is_read_from_the_process_environment_in_one_place() {
        let mut sources = sources_of("capsule-runtime");
        sources.extend(sources_of("murmur-cli"));
        let names_the_variable =
            |args: &&str| args.contains("MURMUR_FORMATION_ID") || args.contains("FORMATION_ID_ENV");
        let mut reads_here = 0;
        for (path, text) in sources {
            let reads = env_reads(crate::source_scan::production_part(&text))
                .into_iter()
                .filter(names_the_variable)
                .count();
            if path == "capsule-runtime/src/formation.rs" {
                reads_here = reads;
            } else {
                assert_eq!(
                    reads, 0,
                    "{path} reads the formation id from the process environment; only \
                     FormationId::from_env does"
                );
            }
        }
        assert_eq!(reads_here, 1, "FormationId::from_env is the one reader");
    }

    #[test]
    fn formation_peers_render_in_order() {
        let peers = FormationPeers::new(vec![
            FormationPeer {
                name: "coder".to_string(),
                url: "http://localhost:41873".to_string(),
            },
            FormationPeer {
                name: "reviewer".to_string(),
                url: "http://reviewer.formation.invalid".to_string(),
            },
        ])
        .unwrap();
        let value = "coder=http://localhost:41873 reviewer=http://reviewer.formation.invalid";
        assert_eq!(peers.render(), value);
        assert_eq!(peers.env_pair(), (FORMATION_PEERS_ENV, value.to_string()));
    }

    #[test]
    fn formation_peers_refuse_every_malformed_peer() {
        let refusal = |peers: &[(&str, &str)]| {
            let peers = peers
                .iter()
                .map(|(name, url)| FormationPeer {
                    name: name.to_string(),
                    url: url.to_string(),
                })
                .collect();
            match FormationPeers::new(peers) {
                Err(error @ RuntimeError::FormationPeersUnreadable { .. }) => error.to_string(),
                other => panic!("expected FormationPeersUnreadable, got {other:?}"),
            }
        };
        for (peers, wording) in [
            (
                &[("Coder", "http://localhost:1")][..],
                "must start with a lowercase letter",
            ),
            (
                &[("co_der", "http://localhost:1")],
                "pair 1 names a member whose name",
            ),
            (
                &[
                    ("coder", "http://localhost:1"),
                    ("coder", "http://localhost:2"),
                ],
                "pair 2 names 'coder' a second time",
            ),
            (
                &[("coder", "https://localhost:1")],
                "does not start with 'http://'",
            ),
            (&[("coder", "localhost:1")], "does not start with 'http://'"),
            (&[("coder", "http://localhost:0")], "port is not a number"),
            (
                &[("coder", "http://localhost:99999")],
                "port is not a number",
            ),
            (&[("coder", "http://localhost:1/x")], "port is not a number"),
            (&[("coder", "http://:1")], "host is not"),
        ] {
            let message = refusal(peers);
            assert!(message.contains(wording), "{peers:?}: {message}");
            assert!(message.contains(FORMATION_PEERS_ENV), "{message}");
        }
        assert!(FormationPeers::new(Vec::new()).is_err());
        // A guest's peers are virtual addresses with no port; a real door always has one.
        assert_eq!(
            peer_url_format_error("http://localhost").as_deref(),
            Some("has a URL with no port")
        );
        assert_eq!(peer_url_format_error("http://localhost:1"), None);
        assert_eq!(peer_url_format_error("http://[::1]:9"), None);
        assert!(peer_url_format_error("http://[::1]").is_some());
    }

    #[test]
    fn formation_peers_in_the_process_environment_are_refused_and_never_read() {
        let _guard = env_guard();
        std::env::remove_var(FORMATION_PEERS_ENV);
        assert!(refuse_formation_peers_in_process_env().is_ok());

        for value in ["coder=http://localhost:7", ""] {
            std::env::set_var(FORMATION_PEERS_ENV, value);
            let refused = refuse_formation_peers_in_process_env();
            std::env::remove_var(FORMATION_PEERS_ENV);
            let message = refused.unwrap_err().to_string();
            assert!(
                message.contains("set in this process's environment"),
                "{message}"
            );
            assert!(!message.contains("localhost"), "{message}");
        }
    }

    #[test]
    fn the_formation_peers_are_read_from_the_process_environment_in_one_place() {
        let mut sources = sources_of("capsule-runtime");
        sources.extend(sources_of("murmur-cli"));
        let names_the_variable = |args: &&str| {
            args.contains("MURMUR_FORMATION_PEERS") || args.contains("FORMATION_PEERS_ENV")
        };
        let mut reads_here = 0;
        for (path, text) in sources {
            let reads = env_reads(crate::source_scan::production_part(&text))
                .into_iter()
                .filter(names_the_variable)
                .count();
            if path == "capsule-runtime/src/formation.rs" {
                reads_here = reads;
            } else {
                assert_eq!(
                    reads, 0,
                    "{path} reads the formation's peers from the process environment; nothing \
                     does but the refusal of a value found there"
                );
            }
        }
        assert_eq!(
            reads_here, 1,
            "refuse_formation_peers_in_process_env is the one place that looks"
        );
    }
}
