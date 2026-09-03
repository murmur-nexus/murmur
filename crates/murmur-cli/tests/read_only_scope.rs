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

/// The contiguous `read_only` block of a surface's stdout: the list itself and, when one is
/// printed, the enforcement sentence under it.
fn read_only_block(stdout: &str) -> String {
    stdout
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("read_only:"))
        .take_while(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("read_only:")
                || trimmed.starts_with("- ")
                || trimmed.starts_with("read_only enforcement:")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `mur doctor` prints the same block, asserted as identity rather than against a second
/// hand-written copy of the wording: a copy is what drifts, and a drifted copy is two surfaces
/// telling an operator two different things about the same declaration.
#[test]
fn run_and_doctor_print_the_same_read_only_block() {
    let home = TempDir::new().unwrap();

    for interpreter in ["git", "python3"] {
        let dir = TempDir::new().unwrap();
        project(dir.path(), &format!("{DECLARED}      - {interpreter}\n"));

        let from_run = read_only_block(&explain_scope(&home, dir.path(), &[]));
        let from_doctor = read_only_block(&doctor_stdout(&home, dir.path()));
        // Load-bearing: two failed extractions are both empty, and equal to each other.
        assert!(
            !from_run.is_empty(),
            "no read_only block on `mur run --explain-scope` stdout for {interpreter}"
        );
        assert!(
            !from_doctor.is_empty(),
            "no read_only block on `mur doctor` stdout for {interpreter}"
        );
        assert_eq!(from_run, from_doctor, "surfaces disagree for {interpreter}");
    }
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
