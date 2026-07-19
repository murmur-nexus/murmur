mod common;

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

// ── Project fixture helpers ───────────────────────────────────────────────────

/// Write a `murmur.yaml` declaring `artifacts_yaml`. `mur doctor` never loads a
/// capsule component, so no `.wasm` is needed.
fn create_project(project_dir: &Path, artifacts_yaml: &str) {
    fs::write(
        project_dir.join("murmur.yaml"),
        format!("name: doctor-fixture\nversion: 0.0.1\nartifacts:\n{artifacts_yaml}"),
    )
    .unwrap();
}

/// A registry-resolvable skill artifact. Skills are platform-agnostic, so this
/// resolves on whatever host the suite runs on.
fn skill_artifact(dir: &TempDir, name: &str, version: &str) -> std::path::PathBuf {
    common::create_skill_artifact(dir.path(), name, version, "# guidance\n")
}

fn mur_doctor(home: &TempDir, project_dir: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
}

/// The platform string the CLI resolves against — the same value `mur run` uses.
/// Asserting on this rather than a hardcoded tuple keeps the suite host-agnostic.
fn platform() -> &'static str {
    murmur_artifact::current_platform()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn doctor_passes_when_declared_artifact_is_in_project_store() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: doctor-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "doctor-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();

    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains("All checks passed."))
        .stdout(predicate::str::contains("\u{2713}  doctor-skill@0.1.0"))
        .stdout(predicate::str::contains(platform()));
}

#[test]
fn doctor_passes_when_declared_artifact_is_only_in_global_store() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: global-skill\n    version: 0.3.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "global-skill", "0.3.0");
    common::publish_local(&home, &artifact).success();

    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains("All checks passed."))
        .stdout(predicate::str::contains("\u{2713}  global-skill@0.3.0"));
}

#[test]
fn doctor_fails_when_declared_artifact_is_in_neither_store() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: absent-skill\n    version: 9.9.9\n    runtime: skill\n",
    );

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout
            .lines()
            .any(|line| line.contains('\u{2717}') && line.contains("absent-skill@9.9.9")),
        "expected a failing line naming absent-skill@9.9.9, got:\n{stdout}"
    );
    assert!(
        stdout.contains("0 checks passed, 1 error found."),
        "expected a pass/fail summary, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Fix: mur install absent-skill@9.9.9"),
        "expected a `mur install` fix hint, got:\n{stdout}"
    );
}

/// The checklist is the manifest: same code, same store, different pin → different verdict.
#[test]
fn doctor_checklist_follows_the_manifest_version_pin() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Only 0.1.0 is ever installed.
    create_project(
        project.path(),
        "  - name: pinned-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "pinned-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();

    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains("All checks passed."));

    // Bump the pin only — no code change, no reinstall.
    create_project(
        project.path(),
        "  - name: pinned-skill\n    version: 0.2.0\n    runtime: skill\n",
    );

    mur_doctor(&home, project.path())
        .failure()
        .stdout(predicate::str::contains("\u{2717}  pinned-skill@0.2.0"))
        .stdout(predicate::str::contains("Fix: mur install pinned-skill@0.2.0"));
}

#[test]
fn doctor_reports_pass_and_fail_counts_across_declared_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: present-skill\n    version: 0.1.0\n    runtime: skill\n\
         \x20 - name: missing-a\n    version: 1.0.0\n    runtime: skill\n\
         \x20 - name: missing-b\n    version: 2.0.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "present-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();

    mur_doctor(&home, project.path())
        .failure()
        .stdout(predicate::str::contains("1 check passed, 2 errors found."))
        .stdout(predicate::str::contains("Fix: mur install missing-a@1.0.0"))
        .stdout(predicate::str::contains("Fix: mur install missing-b@2.0.0"));
}

/// A `source:` skill is resolved from the filesystem at stage time, never a
/// registry — it must never be reported missing.
#[test]
fn doctor_skips_local_source_skills() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::create_dir_all(project.path().join("skills")).unwrap();
    fs::write(project.path().join("skills/skill.md"), "# local\n").unwrap();
    create_project(
        project.path(),
        "  - name: local-skill\n    version: 0.1.0\n    runtime: skill\n    source: ./skills\n",
    );

    let assert = mur_doctor(&home, project.path()).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        !stdout.contains('\u{2717}'),
        "local-source skill must not produce a failing line, got:\n{stdout}"
    );
    assert!(stdout.contains("All checks passed."), "got:\n{stdout}");
}

#[test]
fn doctor_walks_up_to_the_project_root_from_a_subdirectory() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: nested-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "nested-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();

    let nested = project.path().join("src/deep");
    fs::create_dir_all(&nested).unwrap();

    mur_doctor(&home, &nested)
        .success()
        .stdout(predicate::str::contains("\u{2713}  nested-skill@0.1.0"));
}

#[test]
fn doctor_fails_with_e_io_001_when_no_project_root_is_found() {
    let home = tempfile::tempdir().unwrap();
    let empty = tempfile::tempdir().unwrap();

    mur_doctor(&home, empty.path())
        .failure()
        .stderr(predicate::str::contains("error[E-IO-001]"))
        .stderr(predicate::str::contains("no project root found"))
        .stdout(predicate::str::contains("All checks passed.").not());
}

#[test]
fn doctor_surfaces_a_malformed_manifest_before_any_checklist() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(project.path().join("murmur.yaml"), "name: broken\nversion: [\n").unwrap();

    mur_doctor(&home, project.path())
        .failure()
        .stderr(predicate::str::contains("error[E-MAN-002]"))
        .stdout(predicate::str::contains("All checks passed.").not());
}
