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

/// The harness a `transport: process` capsule runs was not one its process driver was tested
/// against, or its version could not be read at all.
///
/// The driver translates for a harness whose flags and output format move between releases, so an
/// untested version may be driven wrongly in ways that only show up mid-run. Nothing is refused:
/// the run proceeds on the driver as written, and the version gap is named instead.
pub const W_RUN_002: &str = "W-RUN-002";

/// An `inference.alternates` driver choice whose credential could not be resolved at launch.
///
/// The primary is what the launch needs, and an alternate is an option, so nothing is refused:
/// the choice is staged unavailable, and a switch to it is refused when it is asked for.
pub const W_RUN_003: &str = "W-RUN-003";

/// A tool's `input_schema` is malformed where the required-field check reads it: the root is not
/// a JSON object, or its `required` is not an array of strings.
///
/// A malformed schema is a bad tool rather than a bad call, so nothing is refused: the check is
/// skipped for that tool, its calls are dispatched unchecked, the session continues, and the
/// malformation is named once per tool per session.
pub const W_RUN_004: &str = "W-RUN-004";

/// A roster edge between two members neither of which is the entry member.
///
/// Only the entry member is handed the addresses of the members it may call, so such an edge is
/// admitted and both members are launched, but the call it allows has no address to travel on.
/// Nothing is refused; the edge is named once per launch.
pub const W_RUN_005: &str = "W-RUN-005";

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
        let codes = [W_RUN_001, W_RUN_002, W_RUN_003, W_RUN_004, W_RUN_005];
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
