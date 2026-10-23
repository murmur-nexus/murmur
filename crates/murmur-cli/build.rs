//! Fixes the commit `mur version` reports at compile time, as `MUR_BUILD_COMMIT`: empty when
//! the build had neither `.cargo_vcs_info.json` nor a git checkout to read it from.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/commit.rs"]
mod commit;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/commit.rs");
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let commit = build_commit(&manifest_dir).unwrap_or_default();
    println!("cargo:rustc-env=MUR_BUILD_COMMIT={commit}");
}

/// A crates.io source package carries its commit in `.cargo_vcs_info.json`; a checkout asks git.
fn build_commit(manifest_dir: &Path) -> Option<String> {
    let vcs_info = manifest_dir.join(".cargo_vcs_info.json");
    if vcs_info.exists() {
        println!("cargo:rerun-if-changed={}", vcs_info.display());
        let json = std::fs::read_to_string(&vcs_info).ok()?;
        return commit::commit_from_vcs_info(&json);
    }
    let commit = commit::commit_from_rev_parse(&git(manifest_dir, &["rev-parse", "HEAD"])?)?;
    for watched in head_files(manifest_dir) {
        println!("cargo:rerun-if-changed={}", watched.display());
    }
    Some(commit)
}

/// The files a commit, checkout or reset rewrites: `HEAD`, the branch it names and
/// `packed-refs`. `--git-path` resolves them in a linked worktree, where `.git` is a file.
/// Only existing ones are returned: cargo counts a missing `rerun-if-changed` path as changed
/// on every build, which would recompile `murmur-cli` each time.
fn head_files(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut names = vec!["HEAD".to_string()];
    if let Some(branch) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(branch.trim().to_string());
    }
    names.push("packed-refs".to_string());
    names
        .iter()
        .filter_map(|name| git(manifest_dir, &["rev-parse", "--git-path", name]))
        .map(|path| manifest_dir.join(path.trim()))
        .filter(|path| path.exists())
        .collect()
}

/// `git <args>`'s stdout, run in `dir`; `None` when git is missing or the command fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
