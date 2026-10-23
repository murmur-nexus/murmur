//! What `scripts/check-dev-version.sh` answers, driven against throwaway git repositories.
//!
//! Each test builds a repository in a temporary directory holding only a `Cargo.toml` with a
//! `[workspace.package]` version, tags it as a release would, and runs the script there. Git
//! reads no global or system configuration, so a signing or hook setting on the host cannot
//! change a commit or a tag.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The checkout this test file was compiled from, which is where the script lives.
fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/check-dev-version.sh")
        .canonicalize()
        .unwrap()
}

fn isolated_git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Dev Version Test",
            "-c",
            "user.email=dev-version@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ]);
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = isolated_git(dir).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn workspace_manifest(version: &str) -> String {
    format!(
        "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"{version}\"\nedition = \"2021\"\n"
    )
}

/// A repository whose one commit sets the workspace version to `version`.
fn repo_at(version: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    commit_manifest(dir.path(), &workspace_manifest(version), "bump");
    dir
}

fn commit_manifest(dir: &Path, manifest: &str, message: &str) {
    fs::write(dir.join("Cargo.toml"), manifest).unwrap();
    git(dir, &["add", "Cargo.toml"]);
    git(dir, &["commit", "--quiet", "-m", message]);
}

fn check(dir: &Path) -> Output {
    Command::new("sh")
        .arg(script())
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_version_no_tag_carries_passes() {
    let repo = repo_at("1.1.0-dev");
    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1.1.0-dev"),
        "{output:?}"
    );
}

#[test]
fn a_lightweight_tag_on_head_passes() {
    let repo = repo_at("1.0.0");
    git(repo.path(), &["tag", "v1.0.0"]);
    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn an_annotated_tag_on_head_passes() {
    let repo = repo_at("1.0.0");
    git(repo.path(), &["tag", "-a", "v1.0.0", "-m", "v1.0.0"]);
    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn a_commit_past_the_tag_with_its_version_fails_naming_the_tag_and_the_next_dev() {
    let repo = repo_at("1.0.0");
    git(repo.path(), &["tag", "v1.0.0"]);
    let tagged = git(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &["commit", "--quiet", "--allow-empty", "-m", "after"],
    );
    let head = git(repo.path(), &["rev-parse", "HEAD"]);

    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("v1.0.0"), "{stderr}");
    assert!(stderr.contains(&tagged), "{stderr}");
    assert!(stderr.contains(&head), "{stderr}");
    assert!(stderr.contains("1.1.0-dev"), "{stderr}");
}

#[test]
fn reopening_main_at_the_next_minor_dev_passes() {
    let repo = repo_at("1.0.0");
    git(repo.path(), &["tag", "v1.0.0"]);
    commit_manifest(repo.path(), &workspace_manifest("1.1.0-dev"), "reopen");
    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn a_version_outside_workspace_package_is_not_read() {
    let repo = repo_at("1.0.0");
    git(repo.path(), &["tag", "v1.0.0"]);
    commit_manifest(
        repo.path(),
        "[package]\nname = \"x\"\nversion = \"1.0.0\"\n\n[workspace.package]\nedition = \"2021\"\n",
        "no workspace version",
    );
    let output = check(repo.path());
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("[workspace.package]"),
        "{output:?}"
    );
}

#[test]
fn outside_a_git_repository_is_a_usage_error() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.toml"), workspace_manifest("1.0.0")).unwrap();
    let output = check(dir.path());
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("not inside a git work tree"),
        "{output:?}"
    );
}
