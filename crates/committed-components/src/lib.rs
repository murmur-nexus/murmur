//! The WASM components committed to this repository, and whether each still matches the WIT it
//! builds against.
//!
//! [`LIST_PATH`] names every committed `.wasm` once. [`check`] compares each non-frozen
//! component's `murmur:*` import and export names against the `package` declarations of the WIT
//! subtree its line names, and [`rebuild`] rebuilds components from source and copies them over
//! their committed outputs. `scripts/rebuild-components.sh` runs this crate's binary, and the
//! crate's own test runs [`check`], so the binary and `cargo test` print the same report.
//!
//! The comparison is by interface version, never by bytes: a rebuild with another toolchain
//! changes `wasi:*` versions, ThinLTO symbol hashes and embedded `CARGO_HOME` paths without
//! changing anything the host resolves.

mod check;
mod list;
mod rebuild;

pub use check::{check, judge, unlisted_components, CheckReport, Entry, Outcome};
pub use list::{find_component, parse_list, read_list, Component, ListError};
pub use rebuild::{rebuild, rebuild_selection, wasm_target, RebuildOutcome, RebuildReport};

/// The list of committed components, relative to the repository root.
pub const LIST_PATH: &str = "crates/capsule-runtime/wit/components.list";

/// The command a developer runs, from the repository root, to rebuild or check the components.
pub const COMMAND: &str = "scripts/rebuild-components.sh";

/// The target every component is built for.
pub const WASM_TARGET: &str = "wasm32-wasip2";

/// The cargo target directory every rebuild shares, relative to the repository root. Under the
/// root `target/`, which `.gitignore` covers, so no build leaves a `target/` inside a fixture.
pub const TARGET_DIR: &str = "target/committed-components";
