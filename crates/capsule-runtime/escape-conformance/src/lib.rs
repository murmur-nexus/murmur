//! Escape-conformance harness — the release gate for the containment boundary
//! `crates/capsule-runtime/src/sandbox.rs` enforces.
//!
//! Read "The escape-conformance harness" in `docs/content/reference/containment.md` first; it
//! carries the build and run commands, the prerequisites and the exit codes, and `--list-cases`
//! prints the case registry. This module documentation covers only what a reader of the code
//! needs.
//!
//! # What this is for
//!
//! `W-SEC-005` says Linux kernel enforcement is implemented but not team-verified. It has rested
//! on one live run on one host, which is a smoke test rather than verification. The intended
//! replacement is a dated record file produced by this harness on real hardware, per containment
//! class, asserting negative results — and it is the *record*, not a green run, that gates the
//! `W-SEC-005` wording.
//!
//! # Four rules the whole design turns on
//!
//! **Refuse, never skip.** If the host cannot back the class named by `--class`, the harness exits
//! non-zero before running a single case, prints declared/achieved/reason using
//! [`capsule_runtime::containment_shortfall_reason`]'s own wording, and writes **no** record. An
//! absent record must never be confused with a passing one. The same applies to a detected
//! container or pod: a container masked three separate findings during the original investigation,
//! so by default it is a refusal too.
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
//! **Assert the gap, do not leave it unmeasured.** A route the runtime does not contain — the
//! process harness, at every class — still runs its cases, each asserted at the verdict it
//! measurably produces as a known gap. A change that closes the gap fails the suite until the
//! expectation flips in the same change, and a known gap is counted on its own, never as a
//! boundary pass or an escape.
//!
//! # In CI
//!
//! The `contained` job in `.github/workflows/ci.yml` runs the gate at `scoped` and at `sealed` on
//! every pull request, on a runner it has first made able to install both, and uploads each dated
//! record. That runner is a VM, so its records are stamped not citable: they gate the change, and
//! `W-SEC-005` evidence still comes from a bare-host run. The tests in this library are pure and
//! touch neither the host nor `mur`; they run with `cargo test --workspace` and keep the generated
//! manifest, the committed driver and the probe's output in step with the runtime.

pub mod cases;
pub mod driver_artifact;
pub mod harness_protocol;
pub mod host;
pub mod probe;
pub mod probe_guest;
pub mod record;
pub mod runner;
pub mod verdict;
pub mod work_root;
