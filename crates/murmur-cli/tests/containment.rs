//! `mur run`'s containment surface: `--explain-scope`, `--containment`, and the refusal when a
//! declared floor is stronger than the host.
//!
//! Every assertion here is host-independent on purpose, and staying that way took real care once
//! `sealed` became achievable. It is no longer true that no host can reach it: a Linux box with a
//! usable Landlock ABI, unprivileged user namespaces and the shipped `mur-sealed` AppArmor profile
//! does. A test that hardcodes "sealed always refuses" would therefore pass on CI (containers, no
//! profile) and fail on exactly the machine the feature was built for — the worst possible place
//! for a test to break.
//!
//! So the `sealed` cases below **ask the host first**, via `--explain-scope --json`, and assert the
//! branch that host is actually in. Both branches are asserted; neither is skipped.
//!
//! Nothing here claims that `scoped` or `sealed` actually contains anything at the kernel level —
//! that can only be checked by hand on a real Linux host, per
//! `docs/content/reference/sealed-containment-manual-verification.md`.

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

/// What this host reports it can back, read from `--explain-scope --json` rather than assumed.
///
/// Uses a throwaway project so the caller's fixture is untouched, and a declared floor of
/// `advisory` so the probe answer is the only thing that varies.
fn host_achieved_containment() -> String {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: advisory\n");

    let output = mur_run(&home, project.path(), &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    report["achieved_containment"].as_str().unwrap().to_string()
}

/// On a host that cannot back `sealed`, the refusal is `E-CAP-003` and it names the *specific*
/// missing mechanism — the AppArmor profile, `CAP_SYS_ADMIN` inside a container, or the kernel —
/// rather than one fixed sentence. On a host that can, the same manifest launches and gets far
/// enough to fail on the deliberately-invalid component instead.
#[test]
fn sealed_refuses_with_an_actionable_reason_unless_the_host_can_back_it() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: sealed\n");

    if host_achieved_containment() == "sealed" {
        // The gate let it through: it fails later, at the invalid component. Asserting the
        // *absence* of E-CAP-003 is the point — a sealed-capable host must not refuse.
        mur_run(&home, project.path(), &[])
            .failure()
            .stderr(predicate::str::contains("E-CAP-003").not());
        return;
    }

    let assertion = mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("E-CAP-003"))
        .stderr(predicate::str::contains("'sealed'"));

    // Exactly one of the mechanism-specific reasons, never a generic "not supported".
    //
    // Compared against `SealedBlocker::ALL` rather than a hand-written list of substrings. The
    // hand-written version was wrong the moment a variant was added: the refusal was correct and
    // specific, the list had not heard of it, and the failure read as "this host cannot do sealed"
    // when the real defect was in the test. Deriving the expected set from the enum means a new
    // blocker can never make this assert lie again.
    let stderr = String::from_utf8(assertion.get_output().stderr.clone()).unwrap();
    let matched = capsule_runtime::sealed::SealedBlocker::ALL
        .iter()
        .find(|blocker| stderr.contains(&blocker.reason()));
    assert!(
        matched.is_some(),
        "the sealed refusal must be one of SealedBlocker's mechanism-specific reasons, got: \
         {stderr}"
    );

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

    let met = if host_achieved_containment() == "sealed" {
        "floor met: yes"
    } else {
        "floor met: no"
    };
    mur_run(&home, project.path(), &["--explain-scope"])
        .success()
        .stdout(predicate::str::contains("declared:  sealed"))
        .stdout(predicate::str::contains(met));

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
    assert_eq!(
        report["network_allow"],
        serde_json::json!(["https://api.example.com"])
    );
    // `floor_met` follows the host, and the two fields must agree with each other — that
    // consistency is the host-independent claim, not either field's value.
    let achieved_is_sealed = report["achieved_containment"] == "sealed";
    assert_eq!(report["floor_met"], achieved_is_sealed);
    assert_eq!(report["shortfall_reason"].is_null(), achieved_is_sealed);
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

    let assertion = mur_run(&home, project.path(), &["--containment", "sealed"]).failure();
    if host_achieved_containment() != "sealed" {
        assertion.stderr(predicate::str::contains("E-CAP-003"));
    }
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
