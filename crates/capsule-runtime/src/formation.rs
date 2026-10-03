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
//! **The id groups; it grants nothing.** No authorization decision reads it — not the door's
//! Bearer check, not task-origin classification, not peer handoff, not the spawn referee.
//! Reachability between sessions comes from the manifest's peer-service declaration, never from
//! sharing a formation.
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

use std::fmt;

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

/// Serializes the unit tests that set or remove [`FORMATION_ID_ENV`] in this test binary's
/// process environment, which every test thread shares.
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
}
