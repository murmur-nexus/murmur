//! Registry of `W-REG-*` codes for non-fatal warnings about what an artifact store holds.
//!
//! Mirrors [`crate::security_warnings`] and [`crate::build_lints`]: the artifact still resolves
//! and the session still runs, so the code is printed rather than raised. Each code has a
//! matching `#w-reg-nnn` anchor on the diagnostics reference page, so callers append
//! [`registry_warning_link`] instead of re-explaining the issue inline.

/// A native payload resolved from the generic, untagged store path because no payload tagged
/// with this host's platform exists.
///
/// Written by an install that predates platform-tagged metadata: the payload runs here, but
/// nothing on disk records which platform it was built for, so a second platform installed into
/// the same version directory would overwrite it and every host would resolve whichever landed
/// last. Reinstalling the artifact refiles it at its tagged path.
pub const W_REG_001: &str = "W-REG-001";

const DIAGNOSTICS_DOC_URL: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/";

/// Builds the doc link for a `W-REG-*` code, e.g. `.../diagnostics/#w-reg-001`.
pub fn registry_warning_link(code: &str) -> String {
    format!("{DIAGNOSTICS_DOC_URL}#{}", code.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-REG-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [W_REG_001];
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
            registry_warning_link(W_REG_001),
            "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-reg-001"
        );
    }
}
