//! Fixes the commit `mur version` reports at compile time, as `MUR_BUILD_COMMIT`. It comes from
//! a crates.io source package's `.cargo_vcs_info.json`, a git checkout of this workspace, or the
//! stamp `git archive` expands in `build/archive-commit.txt`, and is empty when none gives one.
//! `commit::resolve_build_commit` holds the order.

use std::path::PathBuf;

#[path = "build/commit.rs"]
mod commit;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/commit.rs");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    // The crate sits at `crates/murmur-cli` under the workspace root.
    let workspace_root = manifest_dir.join("../..");
    let resolved = commit::resolve_build_commit(&manifest_dir, &workspace_root);
    for watched in &resolved.watched {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    let commit = resolved.commit.unwrap_or_default();
    println!("cargo:rustc-env=MUR_BUILD_COMMIT={commit}");
}
