mod common;

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use murmur_artifact::{
    sha256_hex, write_lockfile_atomic, LockedArtifact, LockedSha256, MurmurLock, LOCK_VERSION,
};
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

/// `mur install <name@version>` scoped to a project directory (no `-g`), which resolves
/// from the global store and pins the result in `project_dir/murmur.lock`.
fn install_pinned_to_project(
    home: &TempDir,
    project_dir: &Path,
    artifact_ref: &str,
) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .args(["install", artifact_ref])
        .assert()
}

/// Overwrite `project_dir/murmur.lock` with a single hand-built entry. `mur install`
/// writes a correct lock; tests call this afterwards to drift one field from it.
fn write_lock(project_dir: &Path, name: &str, resolved_version: &str, sha256_wasm: &str) {
    let lock_path = project_dir.join("murmur.lock");
    let _ = fs::remove_file(&lock_path);
    write_lockfile_atomic(
        &lock_path,
        &MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: vec![LockedArtifact {
                name: name.to_string(),
                resolved_version: resolved_version.to_string(),
                sha256: LockedSha256 {
                    wasm: sha256_wasm.to_string(),
                },
            }],
        },
    )
    .unwrap();
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

// ── Lock integrity ────────────────────────────────────────────────────────────

/// Without a lockfile there is nothing to verify against — presence stays the whole
/// check, and no output mentions the lock at all.
#[test]
fn doctor_says_nothing_about_the_lock_when_no_lockfile_exists() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: solo-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "solo-skill", "0.1.0");
    // Installed straight from a file, which stores the artifact without pinning a lock.
    common::install_artifact_to_project(project.path(), &artifact).success();
    assert!(!project.path().join("murmur.lock").exists());

    let assert = mur_doctor(&home, project.path()).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(stdout.contains("All checks passed."), "got:\n{stdout}");
    assert!(
        !stdout.contains("lock"),
        "no lockfile means no lock-related output, got:\n{stdout}"
    );
}

/// `mur install` writes the lock; doctor must agree with what it wrote.
#[test]
fn doctor_passes_when_the_lock_matches_the_installed_artifact() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: locked-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "locked-skill", "0.1.0");
    common::publish_local(&home, &artifact).success();
    install_pinned_to_project(&home, project.path(), "locked-skill@0.1.0").success();
    assert!(
        project.path().join("murmur.lock").exists(),
        "a registry-resolved project install must pin murmur.lock"
    );

    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains("\u{2713}  locked-skill@0.1.0"))
        .stdout(predicate::str::contains("All checks passed."));
}

/// The gap this closes: bytes on disk that `mur run` would reject with E-REG-002 must
/// never read as a green doctor line.
#[test]
fn doctor_fails_when_the_lock_sha256_does_not_match_the_installed_bytes() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: drifted-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "drifted-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();
    write_lock(project.path(), "drifted-skill", "0.1.0", &"0".repeat(64));

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.lines().any(|line| line.contains('\u{2717}')
            && line.contains("artifact integrity check failed for drifted-skill@0.1.0")),
        "expected E-REG-002 wording on the failing line, got:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("expected sha256 (murmur.lock): {}", "0".repeat(64))),
        "expected the pinned hash to be shown, got:\n{stdout}"
    );
    assert!(
        stdout.contains("actual sha256 (on disk):"),
        "expected the on-disk hash to be shown, got:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "Fix: drifted-skill: artifact on disk does not match murmur.lock \u{2014} re-publish or delete the lock"
        ),
        "expected the E-REG-002 hint as the fix, got:\n{stdout}"
    );
    assert!(
        stdout.contains("0 checks passed, 1 error found."),
        "the artifact must count as a failure, got:\n{stdout}"
    );
    assert!(!stdout.contains("All checks passed."), "got:\n{stdout}");
}

#[test]
fn doctor_fails_when_the_lock_pins_a_different_version_than_the_manifest() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: repinned-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "repinned-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();
    // Same artifact on disk, but the lock claims a version the manifest does not ask for.
    write_lock(project.path(), "repinned-skill", "0.2.0", &"0".repeat(64));

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.lines().any(|line| line.contains('\u{2717}')
            && line.contains(
                "murmur.lock version mismatch for 'repinned-skill': manifest requested 0.1.0, lock pinned 0.2.0"
            )),
        "expected lock version-mismatch wording, got:\n{stdout}"
    );
    // A version mismatch short-circuits the hash check: one drifted artifact, one failure.
    assert!(
        !stdout.contains("artifact integrity check failed"),
        "a version mismatch must not also report a hash mismatch, got:\n{stdout}"
    );
    assert!(
        stdout.contains("0 checks passed, 1 error found."),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("Fix: repinned-skill: remove the stale murmur.lock entry, then run mur install repinned-skill@0.1.0"),
        "got:\n{stdout}"
    );
}

#[test]
fn doctor_fails_when_the_lock_has_no_entry_for_a_declared_artifact() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: unpinned-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "unpinned-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();
    // A lockfile that pins something else entirely — the declared artifact is unpinned.
    write_lock(project.path(), "other-skill", "9.9.9", &"0".repeat(64));

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.lines().any(|line| line.contains('\u{2717}')
            && line.contains("murmur.lock missing artifact entry for 'unpinned-skill'")),
        "expected E-RUN-003 wording, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Fix: mur install unpinned-skill@0.1.0"),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains("0 checks passed, 1 error found."),
        "got:\n{stdout}"
    );
}

/// A `source:` skill is never registry-resolved and never locked — `mur run` skips the
/// lock lookup for it, so doctor must too, even when the lock has no entry for it.
#[test]
fn doctor_exempts_local_source_skills_from_lock_checks() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: pinned-dep\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "pinned-dep", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();
    let installed_sha = sha256_hex(&fs::read(&artifact).unwrap());
    write_lock(project.path(), "pinned-dep", "0.1.0", &installed_sha);

    // Add a local-source skill the lock knows nothing about.
    fs::create_dir_all(project.path().join("skills")).unwrap();
    fs::write(project.path().join("skills/skill.md"), "# local\n").unwrap();
    create_project(
        project.path(),
        "  - name: pinned-dep\n    version: 0.1.0\n    runtime: skill\n\
         \x20 - name: local-skill\n    version: 0.1.0\n    runtime: skill\n    source: ./skills\n",
    );

    let assert = mur_doctor(&home, project.path()).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout
            .lines()
            .any(|line| line.contains('\u{2713}')
                && line.contains("local-skill")
                && line.contains("local source")),
        "expected a passing local-source line, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("murmur.lock missing artifact entry for 'local-skill'"),
        "a local-source skill must never be looked up in the lock, got:\n{stdout}"
    );
    assert!(stdout.contains("All checks passed."), "got:\n{stdout}");
}

/// An unreadable lockfile is as fatal as an unreadable manifest: nothing is verifiable,
/// so doctor aborts before printing a checklist rather than reporting a false green.
#[test]
fn doctor_surfaces_a_malformed_lockfile_before_any_checklist() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    create_project(
        project.path(),
        "  - name: broken-lock-skill\n    version: 0.1.0\n    runtime: skill\n",
    );
    let artifact = skill_artifact(&staging, "broken-lock-skill", "0.1.0");
    common::install_artifact_to_project(project.path(), &artifact).success();
    fs::write(project.path().join("murmur.lock"), "lock_version: [\n").unwrap();

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert.stderr(predicate::str::contains("error[E-RUN-003]"));

    assert!(
        !stdout.contains("broken-lock-skill@0.1.0"),
        "no checklist line may print, got:\n{stdout}"
    );
    assert!(!stdout.contains("All checks passed."), "got:\n{stdout}");
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
