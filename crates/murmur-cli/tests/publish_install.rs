#[path = "common/mod.rs"]
mod common;

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
        .args(["publish", artifact_path.to_str().unwrap()])
        .assert()
}

/// The `.meta.json` sidecar beside a stored zip, parsed.
fn read_meta(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn run_install_local(artifact_ref: &str, home: &TempDir) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
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
        .stdout(predicate::str::contains("Published local/local-only@0.0.2"));

    // Filed under the `local` scope, the file names carrying the bare name.
    let version_dir = home.path().join(".murmur/artifacts/local/local-only/0.0.2");
    assert!(version_dir.join("local-only-0.0.2.mur.zip").exists());
    assert!(version_dir.join("local-only-0.0.2.sha256").exists());
    let meta = read_meta(&version_dir.join("local-only-0.0.2.meta.json"));
    assert_eq!(meta["meta"]["name"], "local/local-only", "{meta}");
    assert!(
        !home.path().join(".murmur/artifacts/local-only").exists(),
        "nothing is filed at the old flat path"
    );
}

#[test]
fn local_install_is_idempotent_and_repeatable() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "local-install", "0.0.2");

    run_publish_local(&artifact, &home).success();

    run_install_local("local/local-install@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains(
            "Installed local/local-install@0.0.2",
        ));

    run_install_local("local/local-install@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains(
            "Installed local/local-install@0.0.2",
        ));
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
            "artifact local/asymmetry@0.0.2 already exists in the store",
        ));

    run_install_local("local/asymmetry@0.0.2", &home)
        .success()
        .stdout(predicate::str::contains("Installed local/asymmetry@0.0.2"));
}

/// A `local/` artifact comes only from its `.mur.zip` file, so a store miss asks no source and
/// names the file install instead.
#[test]
fn local_install_missing_artifact_returns_not_found_error() {
    let home = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());

    run_install_local("local/missing-local@0.0.2", &home)
        .failure()
        .stderr(predicate::str::contains("error[E-REG-001]:"))
        .stderr(predicate::str::contains(
            "artifact local/missing-local@0.0.2 is not in the store, and no source serves the \
             'local' scope",
        ))
        .stderr(predicate::str::contains(
            "a local/ artifact is installed from its .mur.zip file: mur install \
             ./missing-local-0.0.2.mur.zip",
        ));
}

/// A reference on the command line names its scope; a bare one is refused with the scoped forms.
#[test]
fn install_refuses_a_bare_name_naming_the_scoped_forms() {
    let home = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());

    run_install_local("missing-local@0.0.2", &home)
        .failure()
        .stderr(predicate::str::contains("error[E-REG-007]:"))
        .stderr(predicate::str::contains(
            "'missing-local' is not a full artifact name: an artifact is named <scope>/<name>",
        ))
        .stderr(predicate::str::contains(
            "'local/missing-local' for one installed from a .mur.zip file",
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
        .join(".murmur/artifacts/local/tamper-local/0.0.2/tamper-local-0.0.2.mur.zip");
    fs::write(&stored_zip, b"this-is-not-a-valid-zip").unwrap();

    run_install_local("local/tamper-local@0.0.2", &home)
        .failure()
        .stderr(predicate::str::contains(
            "artifact integrity check failed for local/tamper-local@0.0.2",
        ));
}

/// `mur install ./x.mur.zip` in a project files the artifact as `local/<packed name>` in the
/// project store, with both sidecars, and pins that full name in `murmur.lock`.
#[test]
fn installing_a_file_into_a_project_files_and_locks_it_as_local() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: file-install\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();
    let artifact = create_artifact_fixture(work.path(), "from-file", "0.1.0");

    common::install_artifact_to_project_with_home(project.path(), &home, &artifact, &[])
        .success()
        .stdout(predicate::str::contains(format!(
            "Installed local/from-file@0.1.0 from {}",
            artifact.display()
        )));

    let version_dir = project
        .path()
        .join(".murmur/artifacts/local/from-file/0.1.0");
    assert!(version_dir.join("from-file-0.1.0.mur.zip").is_file());
    assert!(version_dir.join("from-file-0.1.0.sha256").is_file());
    let meta = read_meta(&version_dir.join("from-file-0.1.0.meta.json"));
    assert_eq!(meta["meta"]["name"], "local/from-file", "{meta}");

    let lock = murmur_artifact::read_lockfile(&project.path().join("murmur.lock")).unwrap();
    assert_eq!(lock.lock_version, murmur_artifact::LOCK_VERSION);
    assert_eq!(lock.lock_version, 3);
    let names: Vec<&str> = lock.artifacts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["local/from-file"]);
    assert_eq!(lock.artifacts[0].resolved_version, "0.1.0");
    assert_eq!(
        lock.artifacts[0].origin,
        murmur_artifact::LockOrigin::Operator
    );
}

/// `mur install -g ./x.mur.zip` files the artifact as `local/<packed name>` in the global store.
#[test]
fn installing_a_file_globally_files_it_as_local() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = create_artifact_fixture(work.path(), "global-file", "0.1.0");

    run_install_local(artifact.to_str().unwrap(), &home)
        .success()
        .stdout(predicate::str::contains(
            "Installed local/global-file@0.1.0 from",
        ));

    let version_dir = home
        .path()
        .join(".murmur/artifacts/local/global-file/0.1.0");
    assert!(version_dir.join("global-file-0.1.0.mur.zip").is_file());
    assert!(version_dir.join("global-file-0.1.0.sha256").is_file());
    let meta = read_meta(&version_dir.join("global-file-0.1.0.meta.json"));
    assert_eq!(meta["meta"]["name"], "local/global-file", "{meta}");
}

/// A native payload is filed per platform under its scope directory: the zip and both sidecars
/// carry the platform tag the file name declared.
#[test]
fn installing_a_native_file_files_it_per_platform_under_its_scope() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let built = common::create_native_artifact(
        work.path(),
        "local/native-file",
        "0.1.0",
        "#!/bin/sh\nexit 0\n",
        None,
        None,
    );
    let tagged = work.path().join("native-file-0.1.0-linux-aarch64.mur.zip");
    fs::rename(&built, &tagged).unwrap();

    run_install_local(tagged.to_str().unwrap(), &home)
        .success()
        .stdout(predicate::str::contains(
            "Installed local/native-file@0.1.0 from",
        ));

    let version_dir = home
        .path()
        .join(".murmur/artifacts/local/native-file/0.1.0");
    let stem = "native-file-0.1.0-linux-aarch64";
    assert!(version_dir.join(format!("{stem}.mur.zip")).is_file());
    assert!(version_dir.join(format!("{stem}.sha256")).is_file());
    let meta = read_meta(&version_dir.join(format!("{stem}.meta.json")));
    assert_eq!(meta["meta"]["name"], "local/native-file", "{meta}");
    assert!(
        !version_dir.join("native-file-0.1.0.mur.zip").exists(),
        "a native payload is never filed untagged"
    );
}
