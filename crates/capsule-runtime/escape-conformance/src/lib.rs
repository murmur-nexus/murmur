//! Escape-conformance harness — a hand-run release gate for the containment boundary
//! `crates/capsule-runtime/src/sandbox.rs` enforces.
//!
//! Read `docs/content/reference/escape-conformance-harness.md` first; it carries the exact build
//! and run commands and the full case list. This module documentation covers only what a reader
//! of the code needs.
//!
//! # What this is for
//!
//! `W-SEC-005` says Linux kernel enforcement is implemented but not team-verified. It has rested
//! on one live run on one host, which is a smoke test rather than verification. The intended
//! replacement is a dated record file produced by this harness on real hardware, per containment
//! class, asserting negative results — and it is the *record*, not a green run, that gates the
//! `W-SEC-005` wording.
//!
//! # Three rules the whole design turns on
//!
//! **Refuse, never skip.** If the host cannot back the class named by `--class`, the harness exits
//! non-zero before running a single case, prints declared/achieved/reason using
//! [`capsule_runtime::containment_shortfall_reason`]'s own wording, and writes **no** record. An
//! absent record must never be confused with a passing one. The same applies to a detected
//! container: a container masked three separate findings during the original investigation, so by
//! default it is a refusal too.
//!
//! **Resource exhaustion is its own category.** A fork bomb, a disk filler, a memory hog and an
//! fd exhauster assert *availability*. They never feed the boundary rollup — not in the stdout
//! summary, not in the exit code, not in the record. A reviewer skimming the top line must be
//! able to tell "no boundary was crossed" from "the host got starved" without reading every row.
//!
//! **Encode what is true, not what is wished.** `stat-outside-workdir` asserts SUCCESS, because
//! Landlock does not mediate metadata-only syscalls at any ABI and a suite that claimed otherwise
//! would be less trustworthy than silence. Cases the declared class has no mechanism to back are
//! recorded and left ungraded rather than asserted against a promise the class never made.
//!
//! # Never in CI
//!
//! This package is not a member of the root workspace. That exclusion is the mechanism, not a
//! convention: no runner this project has resolves to the full enforcement tier, so a CI-wired
//! suite would skip its way to green and certify nothing — which is exactly how a non-functional
//! Linux tier came to be documented as merely "unverified".

pub mod cases;
pub mod host;
pub mod probe;
pub mod record;
pub mod runner;
pub mod verdict;
