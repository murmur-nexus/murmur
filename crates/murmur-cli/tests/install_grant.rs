//! `capabilities.install` as `mur run --explain-scope` and `mur doctor` report it.
//!
//! Driven through the real binary: the claim is that an operator can read, before launching
//! anything, which skills and tools a capsule may pull at runtime, and that both surfaces say it
//! in one voice.

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

fn project(dir: &Path, capabilities_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: demo\nversion: 0.1.0\n{capabilities_yaml}"),
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

const DECLARED: &str =
    "capabilities:\n  install:\n    skill: [code-review]\n    tool: [\"lint-*\"]\n";

const DECLARED_LINES: &str = "  install skill:\n    - code-review\n  install tool:\n    - lint-*\n";

const UNDECLARED_LINES: &str = "  install skill: <none>\n  install tool: <none>\n";

/// The `install skill` and `install tool` lines, with their entries, from a surface's stdout.
fn install_block(stdout: &str) -> String {
    let mut block = String::new();
    for line in stdout
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("install skill:"))
    {
        let trimmed = line.trim_start();
        let is_install =
            trimmed.starts_with("install skill:") || trimmed.starts_with("install tool:");
        let is_entry = trimmed.starts_with("- ") && line.starts_with("    ");
        if !(is_install || is_entry) {
            break;
        }
        block.push_str(line);
        block.push('\n');
    }
    block
}

#[test]
fn a_declared_grant_is_listed_directly_after_plan_submit() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), DECLARED);

    let rendered = explain_scope(&home, dir.path(), &[]);
    assert_eq!(install_block(&rendered), DECLARED_LINES, "{rendered}");
    assert!(
        rendered.contains(&format!("  plan submit:      false\n{DECLARED_LINES}")),
        "{rendered}"
    );
}

#[test]
fn an_undeclared_grant_is_listed_as_none() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), "");

    let rendered = explain_scope(&home, dir.path(), &[]);
    assert_eq!(install_block(&rendered), UNDECLARED_LINES, "{rendered}");
    assert!(
        rendered.contains(&format!("  plan submit:      false\n{UNDECLARED_LINES}")),
        "{rendered}"
    );
}

/// Both keys are always arrays, including for a capsule with no `capabilities.install` block: an
/// absent key identifies a runtime that predates the grant, never a capsule that declared none.
#[test]
fn both_json_keys_are_arrays_for_every_capsule() {
    let home = TempDir::new().unwrap();

    let declared = TempDir::new().unwrap();
    project(declared.path(), DECLARED);
    let json: serde_json::Value =
        serde_json::from_str(explain_scope(&home, declared.path(), &["--json"]).trim()).unwrap();
    assert_eq!(json["install_skill"], serde_json::json!(["code-review"]));
    assert_eq!(json["install_tool"], serde_json::json!(["lint-*"]));

    let silent = TempDir::new().unwrap();
    project(silent.path(), "");
    let json: serde_json::Value =
        serde_json::from_str(explain_scope(&home, silent.path(), &["--json"]).trim()).unwrap();
    assert_eq!(json["install_skill"], serde_json::json!([]));
    assert_eq!(json["install_tool"], serde_json::json!([]));
}

/// `mur doctor` prints the same lines, asserted as identity with `--explain-scope` rather than
/// against a second hand-written copy of the wording.
#[test]
fn run_and_doctor_print_the_same_install_lines() {
    let home = TempDir::new().unwrap();

    for capabilities in [DECLARED, ""] {
        let dir = TempDir::new().unwrap();
        project(dir.path(), capabilities);

        let from_run = install_block(&explain_scope(&home, dir.path(), &[]));
        let from_doctor = install_block(&doctor_stdout(&home, dir.path()));
        // Two failed extractions are both empty, and equal to each other.
        assert!(from_run.contains("install tool:"), "{from_run}");
        assert!(from_doctor.contains("install tool:"), "{from_doctor}");
        assert!(from_run.starts_with("  install skill:"), "{from_run}");
        assert_eq!(
            from_run, from_doctor,
            "surfaces disagree for {capabilities:?}"
        );
    }
}

/// A block that names nothing is a manifest error, not an empty grant.
#[test]
fn an_empty_install_block_is_refused_naming_the_key() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), "capabilities:\n  install: {}\n");

    let output = mur(
        &home,
        dir.path(),
        &["run", "--manifest", "murmur.yaml", "--explain-scope"],
    )
    .assert()
    .failure()
    .get_output()
    .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("capabilities.install"), "{stderr}");
}
