//! `mur` gets artifacts from the stores on disk and the sources in `~/.murmur/config.yaml`, and
//! nowhere else: `--registry` is refused by the argument parser of every command, with a hint
//! naming where that command reads or writes, and a `registry:` block in a `murmur.yaml` is
//! refused by every command that reads one.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const INSTALL_HINT: &str = "hint: `mur install` resolves from the global store \
                            ~/.murmur/artifacts/, then the registry.sources in \
                            ~/.murmur/config.yaml";
const PUBLISH_HINT: &str = "hint: `mur publish` writes into the global store ~/.murmur/artifacts/";
const INDEX_HINT: &str = "hint: to choose the artifact index, pass --index <PATH|URL|local>";

const REGISTRY_BLOCK_ERROR: &str =
    "error[E-MAN-003]: murmur.yaml: 'registry' is not a manifest key";
const REGISTRY_BLOCK_HINT: &str = "hint: remove it; artifacts resolve from the project store \
                                   (.murmur/artifacts/), the global store (~/.murmur/artifacts/), \
                                   and then the sources under registry.sources in \
                                   ~/.murmur/config.yaml";

fn mur(home: &TempDir, cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).current_dir(cwd);
    cmd
}

/// Every file under `root`, or none when it does not exist.
fn files_under(root: &Path) -> Vec<std::path::PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

fn assert_refused_flag(assert: assert_cmd::assert::Assert, hint: &str) {
    let output = assert.code(2).get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument '--registry"),
        "clap's unknown-argument error: {stderr}"
    );
    assert_eq!(
        stderr.matches("hint:").count(),
        1,
        "exactly one hint line: {stderr}"
    );
    assert!(stderr.contains(hint), "expected {hint:?} in: {stderr}");
}

// ── --registry is refused by the parser ──────────────────────────────────────

#[test]
fn install_refuses_registry_with_a_hint_naming_the_store_then_the_sources() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: cap\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();

    for args in [
        &["install", "--registry", "local", "foo@1.0.0"][..],
        &[
            "install",
            "-g",
            "--registry",
            "http://127.0.0.1:1",
            "foo@1.0.0",
        ][..],
        &["install", "--registry=local"][..],
    ] {
        assert_refused_flag(mur(&home, project.path()).args(args).assert(), INSTALL_HINT);
    }

    assert!(files_under(&home.path().join(".murmur")).is_empty());
    assert!(files_under(&project.path().join(".murmur")).is_empty());
}

#[test]
fn publish_refuses_registry_with_a_hint_naming_the_global_store() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let artifact = common::create_skill_artifact(work.path(), "my-skill", "1.0.0", "# Skill\n");

    assert_refused_flag(
        mur(&home, work.path())
            .args(["publish", "--registry", "local"])
            .arg(&artifact)
            .assert(),
        PUBLISH_HINT,
    );
    assert!(files_under(&home.path().join(".murmur")).is_empty());
}

#[test]
fn search_refuses_registry_with_a_hint_naming_index() {
    let home = TempDir::new().unwrap();
    for args in [
        &["search", "git", "--registry", "local"][..],
        &["search", "git", "--registry=./artifacts-index.json"][..],
    ] {
        assert_refused_flag(mur(&home, home.path()).args(args).assert(), INDEX_HINT);
    }
}

/// A command that never had `--registry` gets clap's error and no hint.
#[test]
fn a_command_that_never_took_registry_gets_no_hint() {
    let home = TempDir::new().unwrap();
    let output = mur(&home, home.path())
        .args(["list", "--registry", "local"])
        .assert()
        .code(2)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument '--registry'"),
        "{stderr}"
    );
    assert!(!stderr.contains("hint:"), "{stderr}");
}

#[test]
fn help_for_install_publish_and_search_omits_registry() {
    let home = TempDir::new().unwrap();
    for command in ["install", "publish", "search"] {
        let output = mur(&home, home.path())
            .args([command, "--help"])
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("--registry"), "{command}: {stdout}");
    }
    mur(&home, home.path())
        .args(["search", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--index <PATH|URL|local>"));
}

// ── publish writes into the global store ─────────────────────────────────────

/// An API-key-shaped variable in the environment does not change where `mur publish` writes.
#[test]
fn publish_writes_into_the_global_store_whatever_the_environment_holds() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let artifact = common::create_skill_artifact(work.path(), "my-skill", "1.0.0", "# Skill\n");
    let key_name = ["NEXUS", "API", "KEY"].join("_");

    mur(&home, work.path())
        .env(&key_name, "x")
        .arg("publish")
        .arg(&artifact)
        .assert()
        .success()
        .stdout(predicate::str::contains("Published my-skill@1.0.0"));

    let version_dir = home
        .path()
        .join(".murmur/artifacts")
        .join("my-skill")
        .join("1.0.0");
    assert!(
        !files_under(&version_dir).is_empty(),
        "nothing published under {}",
        version_dir.display()
    );
}

// ── a registry: block in murmur.yaml is refused ──────────────────────────────

const CAPSULE_WITH_REGISTRY_BLOCK: &str =
    "name: cap\nversion: 0.1.0\nartifacts:\n  - name: foo\n    version: 1.0.0\n    runtime: tool\nregistry:\n  default: local\n";

fn assert_registry_block_refused(assert: assert_cmd::assert::Assert) {
    assert
        .failure()
        .stderr(predicate::str::contains(REGISTRY_BLOCK_ERROR))
        .stderr(predicate::str::contains(REGISTRY_BLOCK_HINT));
}

fn project_with(manifest: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::write(project.path().join("murmur.yaml"), manifest).unwrap();
    project
}

#[test]
fn install_with_no_argument_refuses_a_registry_block() {
    let home = TempDir::new().unwrap();
    let project = project_with(CAPSULE_WITH_REGISTRY_BLOCK);
    assert_registry_block_refused(mur(&home, project.path()).arg("install").assert());
    assert!(files_under(&project.path().join(".murmur")).is_empty());
    assert!(!project.path().join("murmur.lock").exists());
}

#[test]
fn run_refuses_a_registry_block() {
    let home = TempDir::new().unwrap();
    let project = project_with(CAPSULE_WITH_REGISTRY_BLOCK);
    assert_registry_block_refused(common::run_capsule(
        &home,
        &project.path().join("murmur.yaml"),
    ));
    assert!(files_under(&project.path().join(".murmur")).is_empty());
}

#[test]
fn doctor_refuses_a_registry_block() {
    let home = TempDir::new().unwrap();
    let project = project_with(CAPSULE_WITH_REGISTRY_BLOCK);
    assert_registry_block_refused(mur(&home, project.path()).arg("doctor").assert());
}

#[test]
fn build_refuses_a_registry_block() {
    let home = TempDir::new().unwrap();
    let source = project_with(
        "name: my-skill\nversion: 0.1.0\nruntime: skill\nregistry:\n  default: local\n",
    );
    fs::write(source.path().join("skill.md"), "# Skill\n").unwrap();
    let out = TempDir::new().unwrap();
    assert_registry_block_refused(
        mur(&home, source.path())
            .arg("build")
            .arg(source.path())
            .arg("--output")
            .arg(out.path())
            .assert(),
    );
    assert!(files_under(out.path()).is_empty());
}

/// The refusal comes before every other validation: a manifest that also lacks `name` reports
/// the `registry` block.
#[test]
fn the_registry_block_refusal_comes_before_any_other() {
    let home = TempDir::new().unwrap();
    let project = project_with("version: 0.1.0\nregistry:\n  default: local\n");
    assert_registry_block_refused(mur(&home, project.path()).arg("install").assert());
    assert_registry_block_refused(
        mur(&home, project.path())
            .arg("build")
            .arg(project.path())
            .assert(),
    );
}
