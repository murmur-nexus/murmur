//! A launch staging an artifact that does not fit the WIT level this mur serves is refused
//! `E-WIT-LEVEL-001`, and the hint names the newest version of it in its source's index that does
//! fit, or says why none can be named.
//!
//! GitHub is a loopback stub reached through `MUR_GITHUB_API_BASE`; the `mur` binary, the stores,
//! `mur build` and `mur index` are real.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use capsule_runtime::served_wit_level;
use common::github_release::{mur_index, release_assets, MockGitHub};
use murmur_artifact::{ArtifactMeta, ArtifactName, LocalRegistry, Registry, RuntimeType};
use serde_json::Value;
use tempfile::TempDir;

const REPO: &str = "ana/drivers";
const CONFIG: &str = "registry:\n  sources:\n    - repo: ana/drivers\n";

/// The process driver this mur serves, and the one built against `murmur:driver@0.3.0`, the
/// version before it.
fn current_wasm() -> PathBuf {
    common::fixture_path("process-driver/tool/process-driver.wasm")
}

fn previous_wasm() -> PathBuf {
    common::fixture_path("process-driver-v3/tool/process-driver-v3.wasm")
}

/// `mur` under `home`, run from `cwd`, against the GitHub API at `api_base`.
fn mur(home: &Path, cwd: &Path, api_base: &str) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env("MUR_GITHUB_API_BASE", api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(120));
    command
}

/// `dir/process-driver-1.1.0.mur.zip`, packed by `mur build`, so it carries its built-at label.
fn build_current(dir: &Path) {
    let source = TempDir::new().unwrap();
    fs::copy(current_wasm(), source.path().join("process-driver.wasm")).unwrap();
    fs::write(
        source.path().join("murmur.yaml"),
        "name: process-driver\nversion: 1.1.0\nruntime: driver\nrequires_files:\n  - \
         process-driver.wasm\n",
    )
    .unwrap();
    let output = dir.join("process-driver-1.1.0.mur.zip");
    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", source.path().to_str().unwrap(), "-o"])
        .arg(&output)
        .assert()
        .success();
}

/// `dir/process-driver-1.0.0.mur.zip`, packed by hand the way artifacts were before `mur build`
/// labelled them.
fn pack_previous(dir: &Path) -> PathBuf {
    common::create_driver_artifact_with_auth(dir, "process-driver", "1.0.0", &previous_wasm(), "")
}

/// Put `file`, built against a level this mur does not serve, in `home`'s global store as
/// `ana/process-driver@1.0.0` from owner 1111, as a `mur` with no install fit check stored it:
/// `mur install` refuses to.
fn seed_global(home: &Path, file: &Path) {
    let meta = ArtifactMeta {
        name: ArtifactName::parse("ana/process-driver").unwrap(),
        version: "1.0.0".to_string(),
        runtime: RuntimeType::Wasm,
        artifact_runtime: "driver".to_string(),
        platforms: Vec::new(),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: Some(1111),
        built_at_level: None,
    };
    LocalRegistry::new(home.join(".murmur/artifacts"))
        .publish(meta, &fs::read(file).unwrap())
        .unwrap();
}

/// Publish every `.mur.zip` in `dir` as release `tag` of `REPO`, with its `mur index` output.
fn publish(stub: &MockGitHub, tag: &str, dir: &Path) -> Value {
    let index = mur_index(dir, REPO, tag, None);
    let mut assets = release_assets(dir);
    assets.push(("artifacts-index.json".to_string(), index.clone()));
    stub.release(REPO, tag, assets);
    serde_json::from_slice(&index).unwrap()
}

/// A home whose config names `REPO` as the `ana` source, and a `transport: process` project
/// naming `driver@1.0.0`.
fn fixture(driver: &str) -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    fs::create_dir_all(home.path().join(".murmur")).unwrap();
    fs::write(home.path().join(".murmur/config.yaml"), CONFIG).unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: level-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    version: \
             1.0.0\n    runtime: driver\ncapabilities:\n  env:\n    allow: [HOME]\ninference:\n  \
             transport: process\n  driver:\n    artifact: {driver}\n"
        ),
    )
    .unwrap();
    (home, project)
}

/// `mur run`'s stderr, asserting the launch was refused `E-WIT-LEVEL-001` with exit code 1 and the
/// message naming the artifact, its interface and the level.
fn run_refused(home: &Path, project: &Path, api_base: &str, driver: &str) -> String {
    let output = mur(home, project, api_base)
        .args(["run", "--task", "hi"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    println!("{stderr}");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let level = served_wit_level();
    let driver_version = level.version_of("murmur:driver").unwrap();
    for needle in [
        "error[E-WIT-LEVEL-001]".to_string(),
        format!("'{driver}@1.0.0'"),
        format!("does not fit WIT level {}", level.level),
        format!(
            "it exports murmur:driver/process@0.3.0, and level {} has murmur:driver@{driver_version}",
            level.level
        ),
    ] {
        assert!(stderr.contains(&needle), "{needle}: {stderr}");
    }
    assert!(!stderr.contains("E-RUN-029"), "{stderr}");
    stderr
}

/// The hint line of `stderr`.
fn hint(stderr: &str) -> &str {
    stderr
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("hint: "))
        .unwrap_or_else(|| panic!("no hint: {stderr}"))
}

fn sidecar(home: &Path, full: &str, version: &str) -> Value {
    let bare = murmur_artifact::tool_name_of(full);
    let path = home.join(format!(
        ".murmur/artifacts/{full}/{version}/{bare}-{version}.meta.json"
    ));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_launch_names_the_newest_version_of_the_driver_that_fits() {
    let stub = MockGitHub::start();
    stub.repository(REPO, 1111);
    let staging = TempDir::new().unwrap();
    build_current(staging.path());
    let previous = pack_previous(staging.path());
    let index = publish(&stub, "v2", staging.path());

    // The built artifact's label is in the index as written; the hand-packed one is given the
    // newest level its interfaces fit: level 1, the last to have `murmur:driver@0.3.0`.
    let versions = &index["artifacts"][0]["versions"];
    assert_eq!(versions[0]["version"], "1.1.0");
    assert_eq!(versions[0]["built_at_level"], served_wit_level().level);
    assert_eq!(versions[1]["version"], "1.0.0");
    assert_eq!(versions[1]["built_at_level"], 1, "{versions}");

    let (home, project) = fixture("ana/process-driver");
    seed_global(home.path(), &previous);
    assert!(sidecar(home.path(), "ana/process-driver", "1.0.0")["meta"]
        .get("built_at_level")
        .is_none());

    stub.clear_requests();
    let stderr = run_refused(
        home.path(),
        project.path(),
        &stub.api_base,
        "ana/process-driver",
    );
    assert_eq!(
        hint(&stderr),
        format!(
            "ana/process-driver@1.1.0 is the newest version in github:ana/drivers's index that \
             fits level {}; pin it in murmur.yaml and run `mur install`, or run `mur install \
             ana/process-driver@1.1.0`",
            served_wit_level().level
        )
    );
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

#[test]
fn a_launch_says_when_no_version_in_the_index_fits() {
    let stub = MockGitHub::start();
    stub.repository(REPO, 1111);
    let staging = TempDir::new().unwrap();
    let previous = pack_previous(staging.path());
    publish(&stub, "v1", staging.path());

    let (home, project) = fixture("ana/process-driver");
    seed_global(home.path(), &previous);

    stub.clear_requests();
    let stderr = run_refused(
        home.path(),
        project.path(),
        &stub.api_base,
        "ana/process-driver",
    );
    let level = served_wit_level().level;
    assert_eq!(
        hint(&stderr),
        format!(
            "no version of ana/process-driver in github:ana/drivers's index fits level {level}; \
             rebuild it against level {level}'s packages (`mur version` lists them)"
        )
    );
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

#[test]
fn a_launch_says_when_the_index_cannot_be_read() {
    let stub = MockGitHub::start();
    stub.repository(REPO, 1111);
    let staging = TempDir::new().unwrap();
    let previous = pack_previous(staging.path());
    publish(&stub, "v1", staging.path());

    let (home, project) = fixture("ana/process-driver");
    seed_global(home.path(), &previous);

    // A port nothing listens on.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let stderr = run_refused(home.path(), project.path(), &closed, "ana/process-driver");
    let hint = hint(&stderr);
    assert!(
        hint.starts_with("the index of github:ana/drivers could not be read ("),
        "{hint}"
    );
    assert!(
        hint.contains("), so no fitting version can be named; "),
        "{hint}"
    );
}

#[test]
fn a_launch_of_a_driver_installed_from_a_file_asks_no_source() {
    let stub = MockGitHub::start();
    stub.repository(REPO, 1111);
    let staging = TempDir::new().unwrap();
    let file = pack_previous(staging.path());

    let (home, project) = fixture("local/process-driver");
    mur(home.path(), project.path(), &stub.api_base)
        .args(["install", "--no-precompile", "-g"])
        .arg(&file)
        .assert()
        .success();

    stub.clear_requests();
    let stderr = run_refused(
        home.path(),
        project.path(),
        &stub.api_base,
        "local/process-driver",
    );
    let level = served_wit_level().level;
    assert_eq!(
        hint(&stderr),
        format!(
            "local/process-driver was installed from a file, so no source's index can name a \
             version of it that fits level {level}; rebuild it against level {level}'s packages \
             (`mur version` lists them), then run `mur install <file>` on the rebuilt file"
        )
    );
    assert!(stub.paths().is_empty(), "{:?}", stub.paths());
}

#[test]
fn a_launch_says_when_no_source_serves_the_scope() {
    let stub = MockGitHub::start();
    stub.repository(REPO, 1111);
    let staging = TempDir::new().unwrap();
    let previous = pack_previous(staging.path());
    publish(&stub, "v1", staging.path());

    let (home, project) = fixture("ana/process-driver");
    seed_global(home.path(), &previous);
    fs::remove_file(home.path().join(".murmur/config.yaml")).unwrap();

    stub.clear_requests();
    let stderr = run_refused(
        home.path(),
        project.path(),
        &stub.api_base,
        "ana/process-driver",
    );
    let level = served_wit_level().level;
    assert_eq!(
        hint(&stderr),
        format!(
            "no source serves the 'ana' scope, so no index can name a version of \
             ana/process-driver that fits level {level}; add the repository it publishes from \
             under registry.sources and run `mur search ana/process-driver`, or rebuild it \
             against level {level}'s packages (`mur version` lists them)"
        )
    );
    assert!(stub.paths().is_empty(), "{:?}", stub.paths());
}
