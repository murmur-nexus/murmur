//! `capabilities.filesystem.read_only` as `mur run --explain-scope` and `mur doctor` report it.
//!
//! Driven through the real binary for the reason `preopens.rs` is: the claim is that an operator
//! can read, before launching anything, whether the protection they declared is enforced or
//! advisory — and whether the two surfaces say it in one voice.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

fn project(dir: &Path, capabilities_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: read-only-capsule\nversion: 0.1.0\n{capabilities_yaml}"),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn mur(home: &TempDir, project_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .args(args);
    command
}

fn explain_scope(home: &TempDir, project_dir: &Path, extra: &[&str]) -> String {
    let mut args = vec!["run", "--manifest", "murmur.yaml", "--explain-scope"];
    args.extend_from_slice(extra);
    let output = mur(home, project_dir, &args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

fn doctor_stdout(home: &TempDir, project_dir: &Path) -> String {
    let output = mur(home, project_dir, &["doctor"])
        .assert()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

const DECLARED: &str = "capabilities:\n  filesystem:\n    read_only:\n      - tests\n      \
                        - bench/fixtures\n  shell:\n    allow:\n";

/// With no allowlisted interpreter the report states the protection holds everywhere the dispatch
/// check can reach, and claims nothing beyond that.
#[test]
fn a_declaration_with_no_interpreter_renders_as_enforced() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), &format!("{DECLARED}      - git\n"));

    let rendered = explain_scope(&home, dir.path(), &[]);
    assert!(
        rendered.contains("  read_only:\n    - tests\n    - bench/fixtures\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "read_only enforcement: enforced for every tool call and every shell command the \
             dispatch check can read"
        ),
        "{rendered}"
    );
    assert!(!rendered.contains("advisory against"), "{rendered}");
}

/// An allowlisted interpreter is named, and the enforcement sentence is qualified to match what
/// `W-SEC-017` already withdraws — the report must never overstate the boundary.
#[test]
fn an_allowlisted_interpreter_is_named_as_advisory() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), &format!("{DECLARED}      - python3\n"));

    let rendered = explain_scope(&home, dir.path(), &[]);
    assert!(
        rendered.contains("read_only enforcement: advisory against python3"),
        "{rendered}"
    );
}

/// `mur doctor` prints the same block, from the same renderer.
#[test]
fn doctor_states_the_declaration_in_the_same_words() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), &format!("{DECLARED}      - python3\n"));

    let doctor = doctor_stdout(&home, dir.path());
    assert!(doctor.contains("Read-only paths"), "{doctor}");
    assert!(
        doctor.contains(
            "  read_only:\n    - tests\n    - bench/fixtures\n  read_only enforcement: advisory \
             against python3"
        ),
        "{doctor}"
    );
}

/// Both keys are always arrays, including for a capsule that declares no `capabilities.filesystem`
/// block at all: an absent key must identify a runtime that predates the field, never a capsule
/// that declared nothing.
#[test]
fn both_json_keys_are_arrays_for_every_capsule() {
    let home = TempDir::new().unwrap();

    let declared = TempDir::new().unwrap();
    project(declared.path(), &format!("{DECLARED}      - python3\n"));
    let json: serde_json::Value =
        serde_json::from_str(explain_scope(&home, declared.path(), &["--json"]).trim()).unwrap();
    assert_eq!(
        json["read_only_paths"],
        serde_json::json!(["tests", "bench/fixtures"])
    );
    assert_eq!(
        json["read_only_advisory_for"],
        serde_json::json!(["python3"])
    );

    let silent = TempDir::new().unwrap();
    project(silent.path(), "");
    let json: serde_json::Value =
        serde_json::from_str(explain_scope(&home, silent.path(), &["--json"]).trim()).unwrap();
    assert_eq!(json["read_only_paths"], serde_json::json!([]));
    assert_eq!(json["read_only_advisory_for"], serde_json::json!([]));
}
