//! The built-in source of the `murmur` scope, `murmur-nexus/default-artifacts`.
//!
//! With no config file at all, every form of `mur install` resolves a `murmur/` artifact against
//! it without writing a config file. A config file with `registry.sources: []` still has it: the
//! empty list means no third-party scope has a source, so a miss in such a scope fails with
//! `E-REG-008` and opens no connection.
//!
//! The GitHub API is a loopback stub reached through `MUR_GITHUB_API_BASE`.

#[path = "common/mod.rs"]
mod common;

use std::{fs, io::ErrorKind, net::TcpListener, path::Path, time::Duration};

use assert_cmd::Command;
use common::github_release::{publish_dir, wasm_tool_zip, MockGitHub};
use murmur_artifact::{read_lockfile, sha256_hex};
use predicates::prelude::*;
use tempfile::TempDir;

/// The bare name the default source publishes the asset under.
const NAME: &str = "default-source-tool";
/// The full name it installs as: the default source serves the `murmur` scope.
const FULL_NAME: &str = "murmur/default-source-tool";
const VERSION: &str = "0.3.0";
const ABSENT: &str = "ana/absent-artifact@1.0.0";
const DEFAULT_REPO: &str = "murmur-nexus/default-artifacts";
const MURMUR_OWNER_ID: u64 = 190_196_424;

/// `mur` under the scratch `home`, run from `cwd`, with no GitHub token in reach.
fn mur(home: &TempDir, cwd: &Path, api_base: &str) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(60));
    cmd
}

fn write_project(dir: &Path, name: &str, version: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!(
            "name: default-source-project\nversion: 0.0.1\nartifacts:\n  - name: {name}\n    \
             version: {version}\n    runtime: tool\n"
        ),
    )
    .unwrap();
}

fn asset_name() -> String {
    format!("{NAME}-{VERSION}.mur.zip")
}

/// The default source publishing `NAME@VERSION` and its index, built under `staging`.
fn published_release(staging: &Path) -> MockGitHub {
    wasm_tool_zip(staging, NAME, VERSION);
    let stub = MockGitHub::start();
    stub.repository(DEFAULT_REPO, MURMUR_OWNER_ID);
    publish_dir(&stub, DEFAULT_REPO, &format!("v{VERSION}"), staging);
    stub
}

fn assert_asked_default_artifacts(stub: &MockGitHub) {
    let paths = stub.paths();
    assert!(
        !paths.is_empty()
            && paths
                .iter()
                .all(|path| path.starts_with("/repos/murmur-nexus/default-artifacts")),
        "the lookup went to the built-in source; requests: {paths:?}"
    );
}

fn assert_no_config_written(home: &TempDir) {
    assert!(
        !home.path().join(".murmur/config.yaml").exists(),
        "install writes no config file"
    );
}

/// A listener that is bound and never accepted. Returns it with its `MUR_GITHUB_API_BASE`.
fn silent_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    (listener, api_base)
}

/// Whether anything connected to `listener`: a connection the kernel completed sits in the
/// backlog until accepted.
fn was_connected(listener: &TcpListener) -> bool {
    listener.set_nonblocking(true).unwrap();
    match listener.accept() {
        Ok(_) => true,
        Err(err) if err.kind() == ErrorKind::WouldBlock => false,
        Err(err) => panic!("accept on the silent listener failed: {err}"),
    }
}

#[test]
fn a_global_install_with_no_config_file_uses_the_built_in_source() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let stub = published_release(staging.path());

    mur(&home, cwd.path(), &stub.api_base)
        .args(["install", "-g", &format!("{FULL_NAME}@{VERSION}")])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Installed {FULL_NAME}@{VERSION} from github:{DEFAULT_REPO}"
        )));

    assert_asked_default_artifacts(&stub);
    assert!(home
        .path()
        .join(format!(
            ".murmur/artifacts/{FULL_NAME}/{VERSION}/{NAME}-{VERSION}.mur.zip"
        ))
        .exists());
    assert_no_config_written(&home);
}

#[test]
fn a_manifest_install_with_no_config_file_uses_the_built_in_source() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let stub = published_release(staging.path());
    let sha256 = sha256_hex(&fs::read(staging.path().join(asset_name())).unwrap());
    write_project(project.path(), FULL_NAME, VERSION);

    mur(&home, project.path(), &stub.api_base)
        .arg("install")
        .assert()
        .success();

    assert_asked_default_artifacts(&stub);
    assert!(project
        .path()
        .join(format!(
            ".murmur/artifacts/{FULL_NAME}/{VERSION}/{NAME}-{VERSION}.mur.zip"
        ))
        .exists());
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock.artifact_for(FULL_NAME).unwrap();
    assert_eq!(entry.resolved_version, VERSION);
    assert_eq!(entry.sha256.any.as_deref(), Some(sha256.as_str()));
    assert_eq!(entry.owner_id, Some(MURMUR_OWNER_ID));
    assert_no_config_written(&home);
}

#[test]
fn an_empty_source_list_still_serves_the_murmur_scope() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let stub = published_release(staging.path());
    common::write_no_source_config(home.path());

    mur(&home, cwd.path(), &stub.api_base)
        .args(["install", "-g", &format!("{FULL_NAME}@{VERSION}")])
        .assert()
        .success();
    assert_asked_default_artifacts(&stub);
}

#[test]
fn a_global_install_in_an_unserved_scope_fails_without_a_lookup() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());
    let (listener, api_base) = silent_listener();

    mur(&home, cwd.path(), &api_base)
        .args(["install", "-g", ABSENT])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-REG-008]"))
        .stderr(predicate::str::contains(format!(
            "no source serves the 'ana' scope, so {ABSENT} cannot be looked up"
        )))
        .stderr(predicate::str::contains("- repo: ana/<repository>"))
        .stderr(predicate::str::contains("E-REG-006").not());

    assert!(!was_connected(&listener), "no source was asked");
}

#[test]
fn a_manifest_install_in_an_unserved_scope_fails_without_a_lookup() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());
    let (name, version) = ABSENT.split_once('@').unwrap();
    write_project(project.path(), name, version);
    let (listener, api_base) = silent_listener();

    let assert = mur(&home, project.path(), &api_base)
        .arg("install")
        .assert()
        .failure();
    let output = assert.get_output();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("error[E-REG-008]"), "{printed}");
    assert!(
        printed.contains(&format!("{ABSENT} cannot be looked up")),
        "{printed}"
    );
    assert!(!printed.contains("E-REG-006"), "{printed}");

    assert!(!was_connected(&listener), "no source was asked");
}
