//! Registry of `W-RUN-*` codes for non-fatal warnings a session raises while it is running.
//!
//! Mirrors [`crate::security_warnings`], [`crate::registry_warnings`] and
//! [`crate::build_lints`]: the session still produces a result and still ends `ok`, so the code
//! is printed rather than raised. Each code has a matching `#w-run-nnn` anchor on the
//! diagnostics reference page, so callers append [`runtime_warning_link`] instead of
//! re-explaining the issue inline.
//!
//! These fire mid-session, after the loop has been running, so they go to stderr alone. The
//! staging-time `W-SEC-*` warnings also land in `logs/bootstrap.log`, which is closed to writes
//! by the time a turn happens.

/// A turn the provider cut off at the `inference.max_tokens` output cap.
///
/// The reply in `out/result.txt` is a fragment of what the model was writing, not the answer it
/// meant to give. Nothing failed — the provider honoured the cap the capsule asked for — so the
/// session ends `ok` and the truncation is named instead.
pub const W_RUN_001: &str = "W-RUN-001";

const DIAGNOSTICS_DOC_URL: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/";

/// Builds the doc link for a `W-RUN-*` code, e.g. `.../diagnostics/#w-run-001`.
pub fn runtime_warning_link(code: &str) -> String {
    format!("{DIAGNOSTICS_DOC_URL}#{}", code.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-RUN-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [W_RUN_001];
        for code in codes {
            assert!(code.starts_with("W-RUN-"), "malformed code: {code}");
            assert_eq!(code.len(), 9, "malformed code: {code}");
        }
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "two W-RUN codes are equal");
    }

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            runtime_warning_link(W_RUN_001),
            "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-run-001"
        );
    }
}
