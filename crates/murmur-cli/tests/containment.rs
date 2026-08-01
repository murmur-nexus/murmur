//! `mur run`'s containment surface: `--explain-scope`, `--containment`, and the refusal when a
//! declared floor is stronger than the host.
//!
//! Every assertion here is host-independent on purpose. `sealed` is unreachable on *every* host
//! (its mechanism does not exist in this runtime), and `advisory` is satisfied by every host, so
//! these hold identically on macOS and on a Landlock-capable Linux box. Nothing here claims that
//! `scoped` or `sealed` actually contains anything at the kernel level — that can only be checked
//! by hand on a real Linux host, per
//! `.nexus/workspace/builds/f19c4910-manual-verification-procedure.md`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A script capsule: no `inference:` block, so `mur run` looks for a root `*.wasm`. The bytes are
/// a bare module header — enough to be discovered and read, and deliberately not a valid
/// component, so a run that gets *past* the containment gate fails loudly at compile time. That
/// is what lets the "no declaration does not refuse" test below prove the gate was not hit.
fn write_project(dir: &Path, capabilities_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: containment-fixture\nversion: 0.0.1\n{capabilities_yaml}"),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn mur_run(home: &TempDir, project_dir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("run")
        .arg("--manifest")
        .arg("murmur.yaml")
        .args(args)
        .assert()
}

#[test]
fn sealed_refuses_to_launch_on_every_host() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: sealed\n");

    mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("E-CAP-003"))
        .stderr(predicate::str::contains("'sealed'"))
        .stderr(predicate::str::contains("pivot_root"));

    // The refusal lands ahead of workdir creation, so nothing was left behind.
    assert!(
        !project.path().join("workdir").exists(),
        "a refused launch must not create a workdir"
    );
}

#[test]
fn explain_scope_reports_an_unmet_floor_and_still_exits_zero() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: sealed\n");

    mur_run(&home, project.path(), &["--explain-scope"])
        .success()
        .stdout(predicate::str::contains("declared:  sealed"))
        .stdout(predicate::str::contains("floor met: no"));

    assert!(
        !project.path().join("workdir").exists(),
        "--explain-scope must not create a workdir"
    );
}

#[test]
fn explain_scope_json_emits_one_machine_readable_line() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  containment: sealed\n  network:\n    allow:\n      - https://api.example.com\n",
    );

    let output = mur_run(&home, project.path(), &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    assert_eq!(
        stdout.lines().filter(|line| !line.is_empty()).count(),
        1,
        "--json must emit exactly one line: {stdout}"
    );

    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["declared_containment"], "sealed");
    assert_eq!(report["floor_met"], false);
    assert_eq!(
        report["network_allow"],
        serde_json::json!(["https://api.example.com"])
    );
    // Never `sealed`, on any host this suite can run on.
    assert_ne!(report["achieved_containment"], "sealed");
}

#[test]
fn an_undeclared_manifest_is_not_gated() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  shell:\n    allow:\n      - echo\n");

    // Fails later, at the deliberately-invalid component — proving the containment gate let it
    // through rather than refusing a manifest that declared nothing.
    mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("E-RUN-001"))
        .stderr(predicate::str::contains("E-CAP-003").not());
}

#[test]
fn the_cli_flag_can_raise_a_floor_the_manifest_never_declared() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  shell:\n    allow:\n      - echo\n");

    mur_run(&home, project.path(), &["--containment", "sealed"])
        .failure()
        .stderr(predicate::str::contains("E-CAP-003"));
}

#[test]
fn the_workspace_config_can_raise_a_floor_the_manifest_never_declared() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  shell:\n    allow:\n      - echo\n");
    // `project_mur_config_path()` is cwd-relative, and `mur_run` runs in the project dir.
    fs::create_dir_all(project.path().join(".murmur")).unwrap();
    fs::write(
        project.path().join(".murmur").join("config.yaml"),
        "containment: sealed\n",
    )
    .unwrap();

    mur_run(&home, project.path(), &["--explain-scope"])
        .success()
        .stdout(predicate::str::contains("declared:  sealed"));
}

#[test]
fn an_unknown_containment_flag_value_names_the_accepted_set() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  shell:\n    allow:\n      - echo\n");

    mur_run(&home, project.path(), &["--containment", "paranoid"])
        .failure()
        .stderr(predicate::str::contains("E-IO-003"))
        .stderr(predicate::str::contains(
            "--containment must be one of: advisory, scoped, sealed; got 'paranoid'",
        ));
}

#[test]
fn an_unknown_manifest_containment_value_fails_at_parse_time() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: paranoid\n");

    mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("capabilities.containment"))
        .stderr(predicate::str::contains(
            "must be one of: advisory, scoped, sealed",
        ));
}
