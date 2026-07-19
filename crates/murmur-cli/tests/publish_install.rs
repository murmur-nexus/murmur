use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
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

fn run_install_local(artifact_ref: &str, home: &TempDir) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["install", "-g", artifact_ref])
        .assert()
}

#[test]
fn local_publish_writes_zip_and_sha_sidecar_without_http() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "local-only", "0.0.2");

    run_publish_local(&artifact, &home)
        .success()
        .stdout(predicate::str::contains("Published local-only@0.0.2"));

    let published_path = home
        .path()
        .join(".murmur/artifacts/local-only/0.0.2/local-only-0.0.2.mur.zip");
    let sidecar_path = home
        .path()
        .join(".murmur/artifacts/local-only/0.0.2/local-only-0.0.2.sha256");

    assert!(published_path.exists());
    assert!(sidecar_path.exists());
}

#[test]
fn local_install_is_idempotent_and_repeatable() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "local-install", "0.0.2");

    run_publish_local(&artifact, &home).success();

    run_install_local("local-install@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains("Installed local-install@0.0.2"));

    run_install_local("local-install@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains("Installed local-install@0.0.2"));
}

#[test]
fn local_publish_rejects_duplicate_but_install_overwrites() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "asymmetry", "0.0.2");

    run_publish_local(&artifact, &home).success();

    run_publish_local(&artifact, &home)
        .failure()
        .stderr(predicate::str::contains("error[E-REG-003]:"))
        .stderr(predicate::str::contains(
            "artifact asymmetry@0.0.2 already exists in registry",
        ));

    run_install_local("asymmetry@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains("Installed asymmetry@0.0.2"));
}

#[test]
fn local_install_missing_artifact_returns_not_found_error() {
    let home = tempfile::tempdir().unwrap();

    run_install_local("missing-local@0.0.2", &home)
        .failure()
        .stderr(predicate::str::contains(
            "artifact missing-local@0.0.2 not found in registry",
        ));
}

#[test]
fn reserved_versions_are_rejected_locally() {
    for reserved in ["latest", "stable", "edge"] {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let artifact = create_artifact_fixture(work.path(), "reserved", reserved);

        run_publish_local(&artifact, &home)
            .failure()
            .stderr(predicate::str::contains("error[E-REG-004]:"))
            .stderr(predicate::str::contains(format!(
                "version '{reserved}' is reserved and cannot be published"
            )));
    }
}

#[test]
fn local_install_of_tampered_artifact_aborts_with_integrity_error() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "tamper-local", "0.0.2");

    run_publish_local(&artifact, &home).success();

    // Overwrite the stored zip with corrupt bytes while the sha256 sidecar still
    // holds the original hash — simulates on-disk tampering.
    let stored_zip = home
        .path()
        .join(".murmur/artifacts/tamper-local/0.0.2/tamper-local-0.0.2.mur.zip");
    fs::write(&stored_zip, b"this-is-not-a-valid-zip").unwrap();

    run_install_local("tamper-local@0.0.2", &home)
        .failure()
        .stderr(predicate::str::contains(
            "artifact integrity check failed for tamper-local@0.0.2",
        ));
}
