use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use murmur_artifact::{
    read_lockfile, sha256_hex, LockedArtifact, LockedSha256, MurmurLock, LOCK_VERSION,
};
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

fn create_artifact_fixture(dir: &Path, name: &str, version: &str) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("payload.txt", options).unwrap();
    writeln!(zip, "hello from fixture").unwrap();

    zip.finish().unwrap();
    artifact_path
}

fn run_publish_local(artifact_path: &Path, home: &TempDir) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", artifact_path.to_str().unwrap()])
        .assert()
}

/// Run `mur install <artifact_ref>` scoped to a project directory (no `-g`), so the
/// install path exercises `murmur.lock` in `project_dir`.
fn run_install_project(
    artifact_ref: &str,
    home: &TempDir,
    project_dir: &Path,
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .args(["install", artifact_ref])
        .assert()
}

/// Run `mur install` with no artifact argument, so the install path exercises
/// `install_manifest_deps` against `project_dir`'s `murmur.yaml`.
fn run_install_manifest_deps(home: &TempDir, project_dir: &Path) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("install")
        .assert()
}

fn init_project(dir: &Path) {
    fs::write(
        dir.join("murmur.yaml"),
        "name: test-project\nversion: 0.0.1\n",
    )
    .unwrap();
}

/// `mur install` with no artifact argument installs every registry-resolved artifact
/// declared in the project's `murmur.yaml`.
#[test]
fn install_no_args_installs_manifest_declared_dependency() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact = create_artifact_fixture(work.path(), "declared-dep", "1.2.3");
    let expected_sha256 = sha256_hex(&fs::read(&artifact).unwrap());
    run_publish_local(&artifact, &home).success();

    fs::write(
        project.path().join("murmur.yaml"),
        "name: dep-project\nversion: 0.0.1\nartifacts:\n  - name: declared-dep\n    version: 1.2.3\n    runtime: tool\n",
    )
    .unwrap();

    run_install_manifest_deps(&home, project.path()).success();

    let installed = project
        .path()
        .join(".murmur/artifacts/declared-dep/1.2.3/declared-dep-1.2.3.mur.zip");
    assert!(
        installed.exists(),
        "manifest-declared dependency should be installed into the project store"
    );

    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock
        .artifact_for("declared-dep")
        .expect("lock entry for declared-dep");
    assert_eq!(entry.resolved_version, "1.2.3");
    assert_eq!(entry.sha256.any.as_deref().unwrap(), expected_sha256);
}

/// Write a project `murmur.yaml` declaring every `(name, version)` in `deps` as an artifact.
fn write_manifest_with_deps(dir: &Path, deps: &[(&str, &str)]) {
    let mut manifest = String::from("name: dep-project\nversion: 0.0.1\nartifacts:\n");
    for (name, version) in deps {
        manifest.push_str(&format!(
            "  - name: {name}\n    version: {version}\n    runtime: tool\n"
        ));
    }
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
}

/// Publish `name@version` to the registry rooted at `home` and return its sha256.
fn publish_fixture(work: &TempDir, home: &TempDir, name: &str, version: &str) -> String {
    let artifact = create_artifact_fixture(work.path(), name, version);
    let sha256 = sha256_hex(&fs::read(&artifact).unwrap());
    run_publish_local(&artifact, home).success();
    sha256
}

/// Assert `name@version` is both stored in the project store and pinned in `murmur.lock`.
fn assert_installed_and_pinned(project: &Path, name: &str, version: &str, sha256: &str) {
    assert!(
        project
            .join(format!(
                ".murmur/artifacts/{name}/{version}/{name}-{version}.mur.zip"
            ))
            .exists(),
        "{name}@{version} should have been stored despite other artifacts failing"
    );
    let lock = read_lockfile(&project.join("murmur.lock")).unwrap();
    let entry = lock
        .artifact_for(name)
        .unwrap_or_else(|| panic!("lock entry for {name}"));
    assert_eq!(entry.resolved_version, version);
    assert_eq!(entry.sha256.any.as_deref().unwrap(), sha256);
}

/// One unpublished artifact must not discard the other two: both successes are stored *and*
/// pinned, and the failure is named with its own error line on stdout.
#[test]
fn install_partial_failure_pins_successes_and_names_failure() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let sha_a = publish_fixture(&work, &home, "partial-a", "1.0.0");
    let sha_c = publish_fixture(&work, &home, "partial-c", "3.0.0");
    write_manifest_with_deps(
        project.path(),
        &[
            ("partial-a", "1.0.0"),
            ("partial-missing", "2.0.0"),
            ("partial-c", "3.0.0"),
        ],
    );

    run_install_manifest_deps(&home, project.path())
        .failure()
        .stdout(predicate::str::contains("partial-missing@2.0.0"))
        .stdout(predicate::str::contains("error[E-REG-001]:"));

    assert_installed_and_pinned(project.path(), "partial-a", "1.0.0", &sha_a);
    assert_installed_and_pinned(project.path(), "partial-c", "3.0.0", &sha_c);
    assert!(
        !project
            .path()
            .join(".murmur/artifacts/partial-missing")
            .exists(),
        "the unpublished artifact must not be stored"
    );
}

/// Two failures must both be reported — the old `.collect::<Result<_, _>>()` surfaced only
/// the lowest-index one and silently dropped the rest.
#[test]
fn install_multi_failure_reports_every_failing_artifact() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let sha_0 = publish_fixture(&work, &home, "multi-ok-0", "1.0.0");
    let sha_2 = publish_fixture(&work, &home, "multi-ok-2", "1.0.0");
    write_manifest_with_deps(
        project.path(),
        &[
            ("multi-ok-0", "1.0.0"),
            ("multi-gone-1", "1.0.0"),
            ("multi-ok-2", "1.0.0"),
            ("multi-gone-3", "1.0.0"),
        ],
    );

    let assert = run_install_manifest_deps(&home, project.path())
        .failure()
        .stdout(predicate::str::contains("multi-gone-1@1.0.0"))
        .stdout(predicate::str::contains("multi-gone-3@1.0.0"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(
        stdout.matches("error[E-REG-001]:").count(),
        2,
        "each failing artifact gets its own error line; got:\n{stdout}"
    );

    assert_installed_and_pinned(project.path(), "multi-ok-0", "1.0.0", &sha_0);
    assert_installed_and_pinned(project.path(), "multi-ok-2", "1.0.0", &sha_2);
}

/// When every artifact fails there is nothing to pin and nothing to roll up: no lockfile,
/// no store entries, no success line — but both failures are still named.
#[test]
fn install_total_failure_writes_no_lock_and_names_every_failure() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    write_manifest_with_deps(
        project.path(),
        &[("total-gone-a", "1.0.0"), ("total-gone-b", "2.0.0")],
    );

    run_install_manifest_deps(&home, project.path())
        .failure()
        .stdout(predicate::str::contains("total-gone-a@1.0.0"))
        .stdout(predicate::str::contains("total-gone-b@2.0.0"))
        .stdout(predicate::str::contains(
            "2 of 2 artifacts failed to install:",
        ))
        .stdout(predicate::str::contains("installed ").not());

    assert!(
        !project.path().join("murmur.lock").exists(),
        "no successes means murmur.lock is never created"
    );
    assert!(
        fs::read_dir(project.path().join(".murmur/artifacts"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "no artifacts should have been stored"
    );
}

/// `find_project_root` walks up to the directory holding `murmur.yaml`, so a nested CWD
/// still installs into the root's `.murmur/artifacts/`.
#[test]
fn install_from_nested_subdirectory_finds_project_root() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let nested = project.path().join("src").join("deep");
    fs::create_dir_all(&nested).unwrap();

    let artifact = create_artifact_fixture(work.path(), "nested-tool", "1.0.0");
    run_publish_local(&artifact, &home).success();

    run_install_project("nested-tool@1.0.0", &home, &nested).success();

    assert!(
        project
            .path()
            .join(".murmur/artifacts/nested-tool/1.0.0/nested-tool-1.0.0.mur.zip")
            .exists(),
        "artifact should land in the project root store"
    );
    assert!(
        !nested.join(".murmur").exists(),
        "no store should be created in the nested subdirectory"
    );
}

/// A project carrying only the old `manifest.yaml` name is not a project root: there is no
/// dual-name fallback, and the error names `murmur.yaml` only.
#[test]
fn install_with_only_legacy_manifest_name_fails_without_fallback() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("manifest.yaml"),
        "name: legacy-project\nversion: 0.0.1\n",
    )
    .unwrap();

    let artifact = create_artifact_fixture(work.path(), "legacy-tool", "1.0.0");
    run_publish_local(&artifact, &home).success();

    run_install_project("legacy-tool@1.0.0", &home, project.path())
        .failure()
        .stderr(predicate::str::contains("error[E-IO-001]:"))
        .stderr(predicate::str::contains("no project root found"))
        .stderr(predicate::str::contains("murmur.yaml"))
        .stderr(predicate::str::contains("manifest.yaml").not());

    assert!(
        !project.path().join(".murmur").exists(),
        "no project store should be created without a murmur.yaml root"
    );
}

#[test]
fn install_registers_lock_entry_for_registry_resolved_artifact() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let artifact = create_artifact_fixture(work.path(), "locked-tool", "1.0.0");
    let expected_sha256 = sha256_hex(&fs::read(&artifact).unwrap());

    run_publish_local(&artifact, &home).success();

    run_install_project("locked-tool@1.0.0", &home, project.path())
        .success()
        .stdout(predicate::str::contains("Installed locked-tool@1.0.0"));

    let lock_path = project.path().join("murmur.lock");
    assert!(lock_path.exists(), "murmur.lock should have been created");

    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock
        .artifact_for("locked-tool")
        .expect("lock entry for locked-tool");
    assert_eq!(entry.resolved_version, "1.0.0");
    assert_eq!(entry.sha256.any.as_deref().unwrap(), expected_sha256);

    let installed = project
        .path()
        .join(".murmur/artifacts/locked-tool/1.0.0/locked-tool-1.0.0.mur.zip");
    assert!(installed.exists());
}

#[test]
fn install_upserts_lock_preserving_existing_entries() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let lock_path = project.path().join("murmur.lock");
    let preexisting = MurmurLock {
        lock_version: LOCK_VERSION,
        artifacts: vec![LockedArtifact {
            name: "already-pinned".to_string(),
            resolved_version: "0.4.0".to_string(),
            sha256: LockedSha256::any("preexisting-hash".to_string()),
        }],
    };
    murmur_artifact::write_lockfile_atomic(&lock_path, &preexisting).unwrap();

    let artifact = create_artifact_fixture(work.path(), "new-tool", "2.0.0");
    let expected_sha256 = sha256_hex(&fs::read(&artifact).unwrap());
    run_publish_local(&artifact, &home).success();

    run_install_project("new-tool@2.0.0", &home, project.path()).success();

    let lock = read_lockfile(&lock_path).unwrap();
    assert_eq!(lock.artifacts.len(), 2, "existing entry must be preserved");

    let existing = lock.artifact_for("already-pinned").unwrap();
    assert_eq!(existing.resolved_version, "0.4.0");
    assert_eq!(existing.sha256.any.as_deref().unwrap(), "preexisting-hash");

    let new_entry = lock.artifact_for("new-tool").unwrap();
    assert_eq!(new_entry.resolved_version, "2.0.0");
    assert_eq!(new_entry.sha256.any.as_deref().unwrap(), expected_sha256);
}

#[test]
fn install_rejects_lock_conflict_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let artifact = create_artifact_fixture(work.path(), "conflict-tool", "1.0.0");
    run_publish_local(&artifact, &home).success();

    let lock_path = project.path().join("murmur.lock");
    let pinned = MurmurLock {
        lock_version: LOCK_VERSION,
        artifacts: vec![LockedArtifact {
            name: "conflict-tool".to_string(),
            resolved_version: "1.0.0".to_string(),
            sha256: LockedSha256::any("a-completely-different-hash-from-a-prior-pull".to_string()),
        }],
    };
    murmur_artifact::write_lockfile_atomic(&lock_path, &pinned).unwrap();

    run_install_project("conflict-tool@1.0.0", &home, project.path())
        .failure()
        .stderr(predicate::str::contains("error[E-REG-005]:"))
        .stderr(predicate::str::contains("murmur.lock conflict"));

    // Nothing was written to the project-local store.
    let installed_dir = project.path().join(".murmur/artifacts/conflict-tool");
    assert!(!installed_dir.exists());

    // The lock is untouched.
    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock.artifact_for("conflict-tool").unwrap();
    assert_eq!(
        entry.sha256.any.as_deref().unwrap(),
        "a-completely-different-hash-from-a-prior-pull"
    );
}
