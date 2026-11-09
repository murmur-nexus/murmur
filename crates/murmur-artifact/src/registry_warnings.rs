//! Registry of `W-REG-*` codes for non-fatal warnings about what an artifact store holds.
//!
//! Mirrors [`crate::security_warnings`] and [`crate::build_lints`]: the command printing the code
//! refuses nothing over it, so the code is printed rather than raised. Each code has a
//! matching `#w-reg-nnn` anchor on the diagnostics reference page, so callers append
//! [`registry_warning_link`] instead of re-explaining the issue inline.

// W-REG-001 is retired and its code is not reused.

/// A capsule named in a `capabilities.spawn.allow` closure whose own declarations could not be
/// read from either artifact store.
///
/// `mur doctor` walks that closure to report what every capsule in it needs from the operator's
/// environment. A capsule it cannot open contributes nothing to that report, so the report is
/// incomplete — but the walk not being able to read a capsule is not itself evidence that a run
/// fails, so it is named and counted rather than raised.
pub const W_REG_002: &str = "W-REG-002";

/// An installed artifact names a `murmur:` interface version this `mur` does not serve.
///
/// `mur run` refuses it at launch, because the host resolves one instance name per interface and
/// keeps no fallback, and reinstalling the same version fetches the same build. `mur doctor`
/// names it as a warning when the project does not declare it, since an artifact nothing launches
/// fails no run; a declared one fails the checklist instead.
pub const W_REG_003: &str = "W-REG-003";

/// A declared artifact whose `murmur.lock` pin came from a capsule's runtime pull rather than from
/// `mur install`.
///
/// `mur run` stages it, so nothing is refused, but the pin records a session's choice rather than
/// the operator's. `mur doctor` names it with the `mur install` that makes the pin the operator's.
pub const W_REG_004: &str = "W-REG-004";

/// An artifact installed from a source, or held in a store, with no attestation this `mur`
/// verified: no `<asset>.sigstore.json` was published or kept beside it, or the one there is in a
/// form this `mur` does not read.
///
/// `mur install` installs it and names it; nothing is refused, because publishing attestations is
/// not yet required of a source. `mur doctor` reports it for each declared artifact in that state.
pub const W_REG_005: &str = "W-REG-005";

/// An artifact whose verified signer differs from the `signer` its `murmur.lock` entry records,
/// or that `murmur.lock` recorded as attested and that came back with no attestation.
///
/// The bytes still match whatever the lock pins, so nothing is refused: the warning names the
/// workflow that published them now and the one recorded before.
pub const W_REG_006: &str = "W-REG-006";

/// Builds the doc link for a `W-REG-*` code, e.g. `.../diagnostics/#w-reg-001`.
pub fn registry_warning_link(code: &str) -> String {
    crate::diagnostic_link(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-REG-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [W_REG_002, W_REG_003, W_REG_004, W_REG_005, W_REG_006];
        for code in codes {
            assert!(code.starts_with("W-REG-"), "malformed code: {code}");
            assert_eq!(code.len(), 9, "malformed code: {code}");
        }
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "two W-REG codes are equal");
    }

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            registry_warning_link(W_REG_002),
            "https://docs.murmur.nexus/reference/diagnostics/#w-reg-002"
        );
    }
}
