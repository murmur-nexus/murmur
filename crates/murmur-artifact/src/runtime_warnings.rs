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
//!
//! Numbers 005 and 006 are retired and never reused.

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

/// A `mur run` that carries a formation id but was handed no lifeline, and is no delegated child.
///
/// Such a session is a formation member someone started by hand, usually to debug it. It runs as
/// any other `mur run` does, but nothing will wind it down when its formation ends; the warning
/// names the formation and `mur stop` as the way to end it. Printed once, at launch.
pub const W_RUN_007: &str = "W-RUN-007";

/// A formation member the roster lets call another whose `capabilities.network.allow` reaches no
/// loopback `http` door at an unpinned port.
///
/// A roster edge grants a name and a credential, never egress, and a callee's door is served at a
/// port chosen at launch, so every `call-member` call from such a member is refused. Nothing is
/// refused at launch: the member may still be called. Printed once, at staging.
pub const W_RUN_008: &str = "W-RUN-008";

/// A driver choice whose driver's `models:` table lists no limits for its model, in a capsule
/// that sets `inference.max_tokens` but not `context.max_tokens`.
///
/// The output cap is the operator's own, so nothing is refused; with no context window known,
/// compaction does not run and no seed budget applies for that choice. Printed once per choice, at
/// staging, and by `mur doctor` with the same text.
pub const W_RUN_009: &str = "W-RUN-009";

/// `capabilities.shell.allow` names a V8 binary (`node`, `nodejs`) and the address-space limit
/// its commands run under — `capabilities.resources.memory_bytes`, which is `RLIMIT_AS`, declared
/// or its 2 GiB default on a contained tier, declared on an unconfined one — is below 32 GiB.
///
/// V8 reserves far more address space than it uses, so `node` can fail with "Cannot allocate Wasm
/// memory" below that. Nothing is refused. Printed once at staging, and by `mur doctor` with the
/// same text.
pub const W_RUN_010: &str = "W-RUN-010";

/// A tool the capsule pins with an `operator` lock origin declares a need (its `programs:` block)
/// for a program `capabilities.shell.allow` grants, and this host cannot grant it: it resolves to
/// nothing, its program is not on `PATH`, or it resolves to a location no need may reach.
///
/// A missing need never refuses a launch: the program may run without it, or fail for want of it.
/// Printed by `mur doctor`, once per missing need. A need that resolves to nothing names the
/// package the tool declares for this host's distribution; any other names its reason.
pub const W_RUN_011: &str = "W-RUN-011";

/// A tool the capsule pins with an `operator` lock origin pins a download (its `programs:`
/// block's `download:`) of a program `capabilities.shell.allow` grants, and nothing usable runs
/// under that name: no host program of that name is on `PATH` and the program store holds no copy
/// (or the tool pins no download for this platform), or the stored copy fails its checksum, which
/// is named even when a host program runs instead.
///
/// A pinned program never refuses a launch: a command naming it fails as an unknown command
/// would. Printed by `mur doctor`, once per program.
pub const W_RUN_012: &str = "W-RUN-012";

/// `mur doctor` could not tell whether a `transport: process` capsule's harness is logged in for
/// this user: its process driver names no way to ask without a turn, the login command could not
/// be run or did not answer, or the driver could not read what it printed.
///
/// Nothing is refused, and `mur run` never asks: a harness that is logged out fails its first turn
/// with `E-RUN-033`.
pub const W_RUN_013: &str = "W-RUN-013";

/// Builds the doc link for a `W-RUN-*` code, e.g. `.../diagnostics/#w-run-001`.
pub fn runtime_warning_link(code: &str) -> String {
    crate::diagnostic_link(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-RUN-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [
            W_RUN_001, W_RUN_002, W_RUN_003, W_RUN_004, W_RUN_007, W_RUN_008, W_RUN_009, W_RUN_010,
            W_RUN_011, W_RUN_012, W_RUN_013,
        ];
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
            "https://docs.murmur.nexus/reference/diagnostics/#w-run-001"
        );
    }
}
