//! Host-level warnings: stated once per launch, not once per process.
//!
//! A host-level warning is one whose condition and text depend on the host alone — the same for
//! every process on the machine, whatever manifest it runs. `W-SEC-013`
//! ([`crate::runtime::warn_on_userns_restriction_disabled_host_wide`]) is the one there is. A
//! warning gated on, or naming, something in a session's own manifest is a fact about that
//! session and is stated per session, outside this gate.
//!
//! The process the operator started prints each host-level warning once: a plain `mur run` while
//! staging its session, and `mur run --roster` from the launcher before any member starts. Every
//! `mur run` subprocess the runtime starts — each formation member
//! ([`crate::formation_launch`]) and each delegated or plan-step sub-capsule
//! ([`crate::child_launch`]) — is started with [`HOST_WARNINGS_REPORTED_ENV`] set to `1`, telling
//! it that its launcher already did. `mur doctor` reports the host and is never gated.
//!
//! The gate silences stderr only. What a session records about the host — `session_start`'s
//! `userns_grant` — is written whatever this variable holds.

/// Set to `1` by the runtime in the environment of every `mur run` subprocess it starts. Any other
/// value, or none, means this process prints the host-level warnings itself.
pub const HOST_WARNINGS_REPORTED_ENV: &str = "MURMUR_HOST_WARNINGS_REPORTED";

/// The value [`HOST_WARNINGS_REPORTED_ENV`] is set to, and the only one honoured.
pub(crate) const HOST_WARNINGS_REPORTED: &str = "1";

/// Whether this process's launcher already printed the host-level warnings, so this process must
/// not: true only when [`HOST_WARNINGS_REPORTED_ENV`] is exactly `1`.
pub fn host_warnings_reported() -> bool {
    host_warnings_reported_in(std::env::var(HOST_WARNINGS_REPORTED_ENV).ok().as_deref())
}

/// [`host_warnings_reported`] over a given value of [`HOST_WARNINGS_REPORTED_ENV`]: true for
/// exactly `Some("1")`. Unset, empty, `0`, `true` and every other value leave the warnings to this
/// process.
pub fn host_warnings_reported_in(value: Option<&str>) -> bool {
    value == Some(HOST_WARNINGS_REPORTED)
}

/// The `(name, value)` pair the runtime sets in a `mur run` subprocess's environment.
pub(crate) fn reported_env_pair() -> (String, String) {
    (
        HOST_WARNINGS_REPORTED_ENV.to_string(),
        HOST_WARNINGS_REPORTED.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_warnings_are_reported_only_for_exactly_one() {
        assert!(!host_warnings_reported_in(None));
        assert!(!host_warnings_reported_in(Some("")));
        assert!(!host_warnings_reported_in(Some("0")));
        assert!(!host_warnings_reported_in(Some("true")));
        assert!(!host_warnings_reported_in(Some(" 1")));
        assert!(host_warnings_reported_in(Some("1")));
    }

    #[test]
    fn host_warnings_env_pair_is_the_honoured_value() {
        let (name, value) = reported_env_pair();
        assert_eq!(name, "MURMUR_HOST_WARNINGS_REPORTED");
        assert!(host_warnings_reported_in(Some(&value)));
    }
}
