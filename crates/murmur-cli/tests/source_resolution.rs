//! How `mur install` finds an artifact: the one source of its scope, through the
//! `artifacts-index.json` on that source's latest release, matched by exact name and version, and
//! tied to the GitHub account that owns the scope.
//!
//! GitHub is a loopback stub reached through `MUR_GITHUB_API_BASE`; the `mur` binary, the stores,
//! the lock and `mur index` are real.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use capsule_runtime::SERVED_WIT_PACKAGES;
use common::github_release::{
    mur_index, native_tool_zip, publish_dir, release_assets, wasm_tool_zip, write_zip, MockGitHub,
};
use murmur_artifact::{current_platform, read_lockfile, sha256_hex};
use serde_json::json;
use tempfile::TempDir;

const ANA: &str = "ana/artifacts";
const ANA_CONFIG: &str = "registry:\n  sources:\n    - repo: ana/artifacts\n";
const DEFAULT_REPO: &str = "murmur-nexus/default-artifacts";
const MURMUR_OWNER_ID: u64 = 190_196_424;

/// `mur` under `home`, run from `cwd`, against `stub`, with no GitHub token in reach.
fn mur(home: &Path, cwd: &Path, stub: &MockGitHub) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env("MUR_GITHUB_API_BASE", &stub.api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(120));
    command
}

/// `mur install --no-precompile <args>`.
fn install(home: &Path, cwd: &Path, stub: &MockGitHub, args: &[&str]) -> Command {
    let mut command = mur(home, cwd, stub);
    command.args(["install", "--no-precompile"]).args(args);
    command
}

fn write_config(dir: &Path, config: &str) -> PathBuf {
    let murmur = dir.join(".murmur");
    fs::create_dir_all(&murmur).unwrap();
    let path = murmur.join("config.yaml");
    fs::write(&path, config).unwrap();
    path
}

fn write_project(dir: &Path, artifacts: &[(&str, &str)]) {
    let mut manifest = "name: resolution-project\nversion: 0.0.1\nartifacts:\n".to_string();
    for (name, version) in artifacts {
        manifest.push_str(&format!(
            "  - name: {name}\n    version: {version}\n    runtime: tool\n"
        ));
    }
    if artifacts.is_empty() {
        manifest.push_str("  []\n");
    }
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn global_store(home: &Path) -> PathBuf {
    home.join(".murmur").join("artifacts")
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

/// Run `command`, expecting it refused with `code`: a non-zero exit, `error[<code>]` on stderr,
/// and nothing new in `stores`. Returns stderr.
fn refused(mut command: Command, code: &str, stores: &[&Path]) -> String {
    let before: Vec<Vec<String>> = stores.iter().map(|store| files_under(store)).collect();
    let assert = command.assert().failure();
    let stderr = stderr_of(&assert);
    assert!(
        stderr.contains(&format!("error[{code}]")),
        "expected {code}:\n{stderr}\n{}",
        stdout_of(&assert)
    );
    for (store, before) in stores.iter().zip(before) {
        assert_eq!(files_under(store), before, "{} changed", store.display());
    }
    stderr
}

fn assert_contains(text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
    }
}

/// `ana/artifacts`, owner 1111:
/// - `v1` holds `tool-git-0.9.0.mur.zip`;
/// - `v2` (latest) lists `tool-git-validate-2.0.0`, `tool-git-validate-1.0.0`, `tool-git-1.0.0`
///   and an index merged over v1's, so `0.9.0` is kept with `release: v1`.
///
/// Returns the staging directory holding every fixture file.
fn ana_artifacts(stub: &MockGitHub) -> TempDir {
    let staging = tempfile::tempdir().unwrap();
    let v1 = staging.path().join("v1");
    let v2 = staging.path().join("v2");
    fs::create_dir_all(&v1).unwrap();
    fs::create_dir_all(&v2).unwrap();
    wasm_tool_zip(&v1, "tool-git", "0.9.0");
    for (name, version) in [
        ("tool-git-validate", "2.0.0"),
        ("tool-git-validate", "1.0.0"),
        ("tool-git", "1.0.0"),
    ] {
        wasm_tool_zip(&v2, name, version);
    }
    let v1_index = mur_index(&v1, ANA, "v1", None);
    let v1_index_path = staging.path().join("v1-index.json");
    fs::write(&v1_index_path, &v1_index).unwrap();
    let v2_index = mur_index(&v2, ANA, "v2", Some(&v1_index_path));

    stub.repository(ANA, 1111);
    let mut v1_assets = release_assets(&v1);
    v1_assets.push(("artifacts-index.json".to_string(), v1_index));
    stub.release(ANA, "v1", v1_assets);
    let file = |name: &str| (name.to_string(), fs::read(v2.join(name)).unwrap());
    stub.release(
        ANA,
        "v2",
        vec![
            file("tool-git-validate-2.0.0.mur.zip"),
            file("tool-git-validate-1.0.0.mur.zip"),
            file("tool-git-1.0.0.mur.zip"),
            ("artifacts-index.json".to_string(), v2_index),
        ],
    );
    staging
}

fn stored(home: &Path, full: &str, version: &str, file: &str) -> Vec<u8> {
    fs::read(global_store(home).join(full).join(version).join(file)).unwrap()
}

// ─── S1: exact names ─────────────────────────────────────────────────────────

#[test]
fn a_name_resolves_to_exactly_its_own_file() {
    let stub = MockGitHub::start();
    let staging = ana_artifacts(&stub);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);

    let assert = install(
        home.path(),
        cwd.path(),
        &stub,
        &["-g", "ana/tool-git@1.0.0"],
    )
    .assert()
    .success();
    assert_contains(
        &stdout_of(&assert),
        &["Installed ana/tool-git@1.0.0 from github:ana/artifacts"],
    );
    assert_eq!(
        stored(
            home.path(),
            "ana/tool-git",
            "1.0.0",
            "tool-git-1.0.0.mur.zip"
        ),
        fs::read(staging.path().join("v2/tool-git-1.0.0.mur.zip")).unwrap()
    );
    assert_eq!(
        stub.paths(),
        [
            "/repos/ana/artifacts".to_string(),
            "/repos/ana/artifacts/releases/latest".to_string(),
            stub.asset_path(ANA, "v2", "artifacts-index.json"),
            stub.asset_path(ANA, "v2", "tool-git-1.0.0.mur.zip"),
        ]
    );

    stub.clear_requests();
    install(
        home.path(),
        cwd.path(),
        &stub,
        &["-g", "ana/tool-git-validate@1.0.0"],
    )
    .assert()
    .success();
    assert_eq!(
        stored(
            home.path(),
            "ana/tool-git-validate",
            "1.0.0",
            "tool-git-validate-1.0.0.mur.zip"
        ),
        fs::read(staging.path().join("v2/tool-git-validate-1.0.0.mur.zip")).unwrap()
    );

    // No version: the newest that fits, and never a validate file.
    let fresh = tempfile::tempdir().unwrap();
    write_config(fresh.path(), ANA_CONFIG);
    stub.clear_requests();
    let assert = install(fresh.path(), cwd.path(), &stub, &["-g", "ana/tool-git"])
        .assert()
        .success();
    assert_contains(&stdout_of(&assert), &["Installed ana/tool-git@1.0.0"]);
    let validate_paths = [
        stub.asset_path(ANA, "v2", "tool-git-validate-2.0.0.mur.zip"),
        stub.asset_path(ANA, "v2", "tool-git-validate-1.0.0.mur.zip"),
    ];
    assert!(
        stub.paths().iter().all(|p| !validate_paths.contains(p)),
        "{:?}",
        stub.paths()
    );
    assert_eq!(
        files_under(&global_store(fresh.path()).join("ana")),
        [
            "tool-git/1.0.0/tool-git-1.0.0.meta.json",
            "tool-git/1.0.0/tool-git-1.0.0.mur.zip",
            "tool-git/1.0.0/tool-git-1.0.0.sha256",
        ]
    );

    // A version on an older release is fetched from that release.
    stub.clear_requests();
    install(
        home.path(),
        cwd.path(),
        &stub,
        &["-g", "ana/tool-git@0.9.0"],
    )
    .assert()
    .success();
    assert!(stub
        .paths()
        .contains(&"/repos/ana/artifacts/releases/tags/v1".to_string()));
    assert_eq!(
        stored(
            home.path(),
            "ana/tool-git",
            "0.9.0",
            "tool-git-0.9.0.mur.zip"
        ),
        fs::read(staging.path().join("v1/tool-git-0.9.0.mur.zip")).unwrap()
    );
}

#[test]
fn a_version_or_name_the_index_does_not_list_is_refused() {
    let stub = MockGitHub::start();
    let _staging = ana_artifacts(&stub);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);
    let store = global_store(home.path());

    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@2.0.0"],
        ),
        "E-REG-001",
        &[&store],
    );
    assert_contains(
        &stderr,
        &["ana/tool-git@2.0.0", "github:ana/artifacts", "1.0.0, 0.9.0"],
    );

    let stderr = refused(
        install(home.path(), cwd.path(), &stub, &["-g", "ana/tool-gi@1.0.0"]),
        "E-REG-001",
        &[&store],
    );
    assert_contains(&stderr, &["ana/tool-gi@1.0.0", "no artifact 'tool-gi'"]);
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

// ─── S2: a source that claims another scope ─────────────────────────────────

#[test]
fn an_index_claiming_another_scope_is_refused() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    wasm_tool_zip(staging.path(), "tool-git", "1.0.0");

    // ana/claims-bob publishes an index built for bob/x.
    stub.repository("ana/claims-bob", 1111);
    let mut assets = release_assets(staging.path());
    assets.push((
        "artifacts-index.json".to_string(),
        mur_index(staging.path(), "bob/x", "v1", None),
    ));
    stub.release("ana/claims-bob", "v1", assets);

    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(
        home.path(),
        "registry:\n  sources:\n    - repo: ana/claims-bob\n",
    );
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-REG-009",
        &[&global_store(home.path())],
    );
    assert_contains(
        &stderr,
        &[
            "github:ana/claims-bob publishes an index for scope 'bob'",
            "serves scope 'ana'",
        ],
    );
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());

    // murmur-nexus/other publishes an index for the murmur scope.
    stub.repository("murmur-nexus/other", MURMUR_OWNER_ID);
    let mut assets = release_assets(staging.path());
    assets.push((
        "artifacts-index.json".to_string(),
        mur_index(staging.path(), DEFAULT_REPO, "v1", None),
    ));
    stub.release("murmur-nexus/other", "v1", assets);
    write_config(
        home.path(),
        "registry:\n  sources:\n    - repo: murmur-nexus/other\n",
    );
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "murmur-nexus/tool-git@1.0.0"],
        ),
        "E-REG-009",
        &[&global_store(home.path())],
    );
    assert_contains(
        &stderr,
        &[
            "scope 'murmur'",
            "murmur-nexus/default-artifacts",
            "scope 'murmur-nexus'",
        ],
    );
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

// ─── S3: owner identity ─────────────────────────────────────────────────────

fn meta_owner_id(store: &Path, full: &str, version: &str, stem: &str) -> Option<u64> {
    let raw = fs::read_to_string(
        store
            .join(full)
            .join(version)
            .join(format!("{stem}.meta.json")),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    value["meta"]["owner_id"].as_u64()
}

#[test]
fn an_owner_other_than_the_lock_records_is_refused() {
    let stub = MockGitHub::start();
    let _staging = ana_artifacts(&stub);
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);
    write_project(
        project.path(),
        &[
            ("ana/tool-git", "1.0.0"),
            ("ana/tool-git-validate", "1.0.0"),
        ],
    );
    let project_store = project.path().join(".murmur").join("artifacts");

    install(home.path(), project.path(), &stub, &[])
        .assert()
        .success();
    let lock_path = project.path().join("murmur.lock");
    let lock = read_lockfile(&lock_path).unwrap();
    for name in ["ana/tool-git", "ana/tool-git-validate"] {
        assert_eq!(
            lock.artifact_for(name).unwrap().owner_id,
            Some(1111),
            "{name}"
        );
    }
    assert_eq!(
        meta_owner_id(&project_store, "ana/tool-git", "1.0.0", "tool-git-1.0.0"),
        Some(1111)
    );
    assert_eq!(
        meta_owner_id(
            &project_store,
            "ana/tool-git-validate",
            "1.0.0",
            "tool-git-validate-1.0.0"
        ),
        Some(1111)
    );

    // The account was renamed and its name registered again.
    stub.set_owner_id(ANA, 2222);
    write_project(
        project.path(),
        &[
            ("ana/tool-git", "0.9.0"),
            ("ana/tool-git-validate", "1.0.0"),
        ],
    );
    let lock_before = fs::read(&lock_path).unwrap();
    stub.clear_requests();
    let stderr = refused(
        install(home.path(), project.path(), &stub, &[]),
        "E-REG-010",
        &[&project_store, &global_store(home.path())],
    );
    assert_contains(
        &stderr,
        &[
            "scope 'ana'",
            "github:ana/artifacts",
            "owner id 1111 (entry ana/tool-git)",
            "account id 2222",
            "renamed",
            "delete the 'ana' entries from murmur.lock",
        ],
    );
    assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());

    // The global store holds ana/tool-git@0.9.0 from the new owner; a project install served from
    // it is refused without a request.
    install(
        home.path(),
        project.path(),
        &stub,
        &["-g", "ana/tool-git@0.9.0"],
    )
    .assert()
    .success();
    assert_eq!(
        meta_owner_id(
            &global_store(home.path()),
            "ana/tool-git",
            "0.9.0",
            "tool-git-0.9.0"
        ),
        Some(2222)
    );
    stub.clear_requests();
    let stderr = refused(
        install(home.path(), project.path(), &stub, &["ana/tool-git@0.9.0"]),
        "E-REG-010",
        &[&project_store],
    );
    assert_contains(
        &stderr,
        &[
            "ana/tool-git@0.9.0 in the global store",
            "account id 2222",
            "owner id 1111",
        ],
    );
    let stderr = refused(
        install(home.path(), project.path(), &stub, &[]),
        "E-REG-010",
        &[&project_store],
    );
    assert_contains(&stderr, &["in the global store"]);
    assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    assert!(stub.paths().is_empty(), "{:?}", stub.paths());
}

/// The default source, `murmur-nexus/default-artifacts`, holding `tool-x@0.1.0` on release
/// `v0.25.0` with a `mur index` index, owned by `owner_id`.
fn default_source(stub: &MockGitHub, owner_id: u64) -> TempDir {
    let staging = tempfile::tempdir().unwrap();
    wasm_tool_zip(staging.path(), "tool-x", "0.1.0");
    stub.repository(DEFAULT_REPO, owner_id);
    publish_dir(stub, DEFAULT_REPO, "v0.25.0", staging.path());
    staging
}

#[test]
fn the_murmur_scope_refuses_any_owner_but_its_own() {
    let stub = MockGitHub::start();
    let _staging = default_source(&stub, 1);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "murmur/tool-x@0.1.0"],
        ),
        "E-REG-010",
        &[&global_store(home.path())],
    );
    assert_contains(&stderr, &["190196424", "owner id 1"]);
    assert_eq!(
        stub.paths(),
        ["/repos/murmur-nexus/default-artifacts".to_string()]
    );
}

// ─── S4: newest fitting ──────────────────────────────────────────────────────

/// `murmur:tool/run` at the version this mur serves.
fn served_tool_run() -> String {
    let (package, version) = SERVED_WIT_PACKAGES
        .iter()
        .find(|(package, _)| *package == "murmur:tool")
        .unwrap();
    format!("{package}/run@{version}")
}

fn other_platform() -> &'static str {
    if current_platform() == "darwin-aarch64" {
        "linux-x86_64"
    } else {
        "darwin-aarch64"
    }
}

/// One version of a hand-written index: `(version, interfaces, [(platform, file, bytes)])`.
type HandVersion<'a> = (
    &'a str,
    Vec<String>,
    Vec<(Option<&'a str>, String, Vec<u8>)>,
);

/// Publish a hand-written schema 2 index of `scope` on release `tag` of `repo`, with the real
/// SHA-256 of every file, and every file as an asset.
fn publish_hand_index(
    stub: &MockGitHub,
    repo: &str,
    scope: &str,
    tag: &str,
    artifacts: Vec<(&str, Vec<HandVersion<'_>>)>,
) {
    let mut assets = Vec::new();
    let mut listed = Vec::new();
    for (name, versions) in artifacts {
        let mut versions_json = Vec::new();
        for (version, interfaces, files) in versions {
            let mut assets_json = Vec::new();
            for (platform, file, bytes) in files {
                let mut asset = json!({ "file": file, "sha256": sha256_hex(&bytes) });
                if let Some(platform) = platform {
                    asset["platform"] = json!(platform);
                }
                assets_json.push(asset);
                assets.push((file, bytes));
            }
            versions_json.push(json!({
                "version": version,
                "release": tag,
                "interfaces": interfaces,
                "assets": assets_json,
            }));
        }
        listed.push(json!({
            "name": name,
            "runtime": "tool",
            "tags": ["tool"],
            "versions": versions_json,
        }));
    }
    let index = json!({ "schema_version": 2, "scope": scope, "artifacts": listed });
    assets.push((
        "artifacts-index.json".to_string(),
        serde_json::to_vec_pretty(&index).unwrap(),
    ));
    stub.release(repo, tag, assets);
}

fn zip_of(dir: &Path, name: &str, version: &str) -> Vec<u8> {
    fs::read(wasm_tool_zip(dir, name, version)).unwrap()
}

#[test]
fn an_unversioned_reference_installs_the_newest_version_that_fits() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    let dir = staging.path();
    let served = served_tool_run();
    let unserved = "murmur:tool/run@9.9.9".to_string();
    stub.repository("ana/fits", 1111);
    publish_hand_index(
        &stub,
        "ana/fits",
        "ana",
        "v1",
        vec![
            (
                "tool-none",
                vec![(
                    "1.0.0",
                    vec![unserved.clone()],
                    vec![(
                        None,
                        "tool-none-1.0.0.mur.zip".to_string(),
                        zip_of(dir, "tool-none", "1.0.0"),
                    )],
                )],
            ),
            (
                "tool-x",
                vec![
                    (
                        "0.3.0",
                        vec![unserved],
                        vec![(
                            None,
                            "tool-x-0.3.0.mur.zip".to_string(),
                            zip_of(dir, "tool-x", "0.3.0"),
                        )],
                    ),
                    (
                        "0.2.0",
                        vec![served.clone()],
                        vec![(
                            Some(other_platform()),
                            format!("tool-x-0.2.0-{}.mur.zip", other_platform()),
                            zip_of(dir, "tool-x", "0.2.0"),
                        )],
                    ),
                    (
                        "0.1.1",
                        vec![served.clone()],
                        vec![(
                            None,
                            "tool-x-0.1.1.mur.zip".to_string(),
                            zip_of(dir, "tool-x", "0.1.1"),
                        )],
                    ),
                    (
                        "0.1.0",
                        vec![served],
                        vec![(
                            None,
                            "tool-x-0.1.0.mur.zip".to_string(),
                            zip_of(dir, "tool-x", "0.1.0"),
                        )],
                    ),
                ],
            ),
        ],
    );

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_config(home.path(), "registry:\n  sources:\n    - repo: ana/fits\n");
    write_project(project.path(), &[]);

    let assert = install(home.path(), project.path(), &stub, &["ana/tool-x"])
        .assert()
        .success();
    assert_contains(
        &stdout_of(&assert),
        &["Installed ana/tool-x@0.1.1 from github:ana/fits"],
    );
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock.artifact_for("ana/tool-x").unwrap();
    assert_eq!(entry.resolved_version, "0.1.1");
    assert_eq!(entry.owner_id, Some(1111));

    let stderr = refused(
        install(home.path(), project.path(), &stub, &["-g", "ana/tool-none"]),
        "E-REG-001",
        &[&global_store(home.path())],
    );
    let (package, version) = SERVED_WIT_PACKAGES
        .iter()
        .find(|(package, _)| *package == "murmur:tool")
        .unwrap();
    assert_contains(
        &stderr,
        &[
            "no version of ana/tool-none in github:ana/fits fits this mur",
            &format!(
                "ana/tool-none: 1.0.0 needs murmur:tool/run@9.9.9, this mur serves \
                 {package}@{version}"
            ),
            "hint: name a version explicitly",
            "mur install ana/tool-none@1.0.0",
        ],
    );

    // An explicit version installs as asked.
    let assert = install(
        home.path(),
        project.path(),
        &stub,
        &["-g", "ana/tool-x@0.3.0"],
    )
    .assert()
    .success();
    assert_contains(&stdout_of(&assert), &["Installed ana/tool-x@0.3.0"]);

    for reserved in ["latest", "stable", "edge"] {
        stub.clear_requests();
        let stderr = refused(
            install(
                home.path(),
                project.path(),
                &stub,
                &["-g", &format!("ana/tool-x@{reserved}")],
            ),
            "E-REG-004",
            &[&global_store(home.path())],
        );
        assert_contains(&stderr, &["omit the version", "mur install ana/tool-x"]);
        assert!(stub.paths().is_empty(), "{:?}", stub.paths());
    }
}

// ─── S5: digests and packed identity ────────────────────────────────────────

#[test]
fn a_payload_other_than_the_index_describes_is_refused() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    let dir = staging.path();
    stub.repository(ANA, 1111);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);
    let store = global_store(home.path());

    // A wrong digest in an index `mur index` wrote.
    let good = dir.join("good");
    fs::create_dir_all(&good).unwrap();
    wasm_tool_zip(&good, "tool-git", "1.0.0");
    let real = sha256_hex(&fs::read(good.join("tool-git-1.0.0.mur.zip")).unwrap());
    let wrong = "f".repeat(64);
    let index = String::from_utf8(mur_index(&good, ANA, "v1", None))
        .unwrap()
        .replace(&real, &wrong);
    let mut assets = release_assets(&good);
    assets.push(("artifacts-index.json".to_string(), index.into_bytes()));
    stub.release(ANA, "v1", assets);
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-REG-002",
        &[&store],
    );
    assert_contains(&stderr, &["tool-git-1.0.0.mur.zip", &real, &wrong]);

    // A file packing another version than the index lists it under.
    let mislabelled = dir.join("mislabelled.mur.zip");
    write_zip(
        &mislabelled,
        "name: tool-git\nversion: 1.0.1\nruntime: tool\n",
        &[("tool.wasm", b"\0asm\x01\0\0\0")],
    );
    publish_hand_index(
        &stub,
        ANA,
        "ana",
        "v2",
        vec![(
            "tool-git",
            vec![(
                "1.0.0",
                Vec::new(),
                vec![(
                    None,
                    "tool-git-1.0.0.mur.zip".to_string(),
                    fs::read(&mislabelled).unwrap(),
                )],
            )],
        )],
    );
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-REG-002",
        &[&store],
    );
    assert_contains(&stderr, &["packs version '1.0.1'", "ana/tool-git@1.0.0"]);

    // A file packing another name.
    write_zip(
        &mislabelled,
        "name: tool-other\nversion: 1.0.0\nruntime: tool\n",
        &[("tool.wasm", b"\0asm\x01\0\0\0")],
    );
    publish_hand_index(
        &stub,
        ANA,
        "ana",
        "v3",
        vec![(
            "tool-git",
            vec![(
                "1.0.0",
                Vec::new(),
                vec![(
                    None,
                    "tool-git-1.0.0.mur.zip".to_string(),
                    fs::read(&mislabelled).unwrap(),
                )],
            )],
        )],
    );
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-REG-002",
        &[&store],
    );
    assert_contains(&stderr, &["declares name 'tool-other'"]);
}

// ─── S6: no config ───────────────────────────────────────────────────────────

#[test]
fn the_murmur_scope_is_served_with_no_config_file() {
    let stub = MockGitHub::start();
    let _staging = default_source(&stub, MURMUR_OWNER_ID);
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    install(
        home.path(),
        project.path(),
        &stub,
        &["-g", "murmur/tool-x@0.1.0"],
    )
    .assert()
    .success();
    assert!(global_store(home.path())
        .join("murmur/tool-x/0.1.0/tool-x-0.1.0.mur.zip")
        .exists());

    let other_home = tempfile::tempdir().unwrap();
    write_project(project.path(), &[("murmur/tool-x", "0.1.0")]);
    install(other_home.path(), project.path(), &stub, &[])
        .assert()
        .success();
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    assert_eq!(
        lock.artifact_for("murmur/tool-x").unwrap().owner_id,
        Some(MURMUR_OWNER_ID)
    );
    for home in [home.path(), other_home.path()] {
        assert!(!home.join(".murmur/config.yaml").exists());
    }
    assert!(!project.path().join(".murmur/config.yaml").exists());
}

#[test]
fn an_empty_source_list_leaves_only_the_murmur_scope() {
    let stub = MockGitHub::start();
    let _staging = default_source(&stub, MURMUR_OWNER_ID);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());

    install(
        home.path(),
        cwd.path(),
        &stub,
        &["-g", "murmur/tool-x@0.1.0"],
    )
    .assert()
    .success();

    stub.clear_requests();
    let stderr = refused(
        install(home.path(), cwd.path(), &stub, &["-g", "ana/tool-x@1.0.0"]),
        "E-REG-008",
        &[&global_store(home.path())],
    );
    assert_contains(
        &stderr,
        &[
            "no source serves the 'ana' scope",
            "ana/tool-x@1.0.0",
            "    registry:\n      sources:\n        - repo: ana/<repository>",
        ],
    );
    assert!(stub.paths().is_empty(), "{:?}", stub.paths());
}

#[test]
fn a_default_artifacts_entry_lends_the_built_in_source_its_token() {
    let stub = MockGitHub::start();
    let _staging = default_source(&stub, MURMUR_OWNER_ID);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(
        home.path(),
        "registry:\n  sources:\n    - repo: murmur-nexus/default-artifacts\n      token: ${MY_TOKEN}\n",
    );

    install(
        home.path(),
        cwd.path(),
        &stub,
        &["-g", "murmur/tool-x@0.1.0"],
    )
    .env("MY_TOKEN", "abc")
    .assert()
    .success();
    let requests = stub.requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert!(
        requests
            .iter()
            .all(|r| r.authorization.as_deref() == Some("Bearer abc")),
        "{requests:?}"
    );
}

// ─── S7: config refusals ─────────────────────────────────────────────────────

#[test]
fn every_removed_config_form_is_refused_naming_the_file() {
    let stub = MockGitHub::start();
    let cwd = tempfile::tempdir().unwrap();
    for (config, needles) in [
        (
            "registry:\n  default: official\n",
            &["sets registry.default"][..],
        ),
        (
            "registry:\n  sources:\n    - name: official\n      repo: ana/a\n",
            &["registry.sources[0] sets name:"][..],
        ),
        (
            "registry:\n  sources:\n    - type: github\n      repo: ana/a\n",
            &["registry.sources[0] sets type:"][..],
        ),
        (
            "registry:\n  sources:\n    - url: https://example.com\n      repo: ana/a\n",
            &["registry.sources[0] sets url:"][..],
        ),
        (
            "registry:\n  sources:\n    - token: abc\n",
            &["registry.sources[0] names no repo"][..],
        ),
        (
            "registry:\n  sources:\n    - repo: ana\n",
            &["registry.sources[0] repo 'ana'"][..],
        ),
        (
            "registry:\n  sources:\n    - repo: ana/x/y\n",
            &["registry.sources[0] repo 'ana/x/y'"][..],
        ),
        (
            "registry:\n  sources:\n    - repo: murmur/x\n",
            &["reserved scope 'murmur'"][..],
        ),
        (
            "registry:\n  sources:\n    - repo: local/x\n",
            &["reserved scope 'local'"][..],
        ),
        (
            "registry:\n  sources:\n    - repo: ana/a\n    - repo: Ana/b\n",
            &[
                "registry.sources[0] (ana/a)",
                "registry.sources[1] (Ana/b)",
                "scope 'ana'",
                "keep one of them",
            ][..],
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let path = write_config(home.path(), config);
        stub.clear_requests();
        let stderr = refused(
            install(
                home.path(),
                cwd.path(),
                &stub,
                &["-g", "ana/tool-git@1.0.0"],
            ),
            "E-CFG-003",
            &[&global_store(home.path())],
        );
        assert_contains(&stderr, &[&path.display().to_string(), "hint: "]);
        assert_contains(&stderr, needles);
        if !needles[0].contains("reserved") && !needles[0].contains("[0] (ana/a)") {
            assert_contains(&stderr, &["- repo: <owner>/<repo>", "token:"]);
        }
        assert!(stub.paths().is_empty(), "{config}: {:?}", stub.paths());
    }
}

#[test]
fn a_project_source_replaces_the_global_one_for_its_scope() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    wasm_tool_zip(staging.path(), "tool-git", "1.0.0");
    stub.repository("ana/b", 1111);
    publish_dir(&stub, "ana/b", "v1", staging.path());

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_config(home.path(), "registry:\n  sources:\n    - repo: ana/a\n");
    write_config(project.path(), "registry:\n  sources:\n    - repo: ana/b\n");

    let assert = install(
        home.path(),
        project.path(),
        &stub,
        &["-g", "ana/tool-git@1.0.0"],
    )
    .assert()
    .success();
    assert_contains(
        &stdout_of(&assert),
        &["Installed ana/tool-git@1.0.0 from github:ana/b"],
    );
    assert!(
        stub.paths()
            .iter()
            .all(|path| !path.starts_with("/repos/ana/a")),
        "{:?}",
        stub.paths()
    );
}

#[test]
fn config_set_refuses_the_removed_default_key() {
    let stub = MockGitHub::start();
    let home = tempfile::tempdir().unwrap();
    let assert = mur(home.path(), home.path(), &stub)
        .args(["config", "set", "-g", "registry.default", "official"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert_contains(&stderr, &["error[E-CFG-002]", "supported keys: "]);
    let hint = stderr.lines().find(|l| l.contains("hint:")).unwrap();
    assert!(!hint.contains("registry.default"), "{hint}");
}

// ─── S8: no schema 2 index ───────────────────────────────────────────────────

#[test]
fn a_source_without_a_schema_2_index_is_refused() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    wasm_tool_zip(staging.path(), "tool-git", "1.0.0");
    stub.repository(ANA, 1111);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);
    let store = global_store(home.path());

    stub.release(ANA, "v2", release_assets(staging.path()));
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-IDX-002",
        &[&store],
    );
    assert_contains(
        &stderr,
        &[
            "github:ana/artifacts publishes no artifacts-index.json on its latest release v2",
            "mur index <dir> --repo ana/artifacts --release v2",
            "uploads artifacts-index.json to release v2",
        ],
    );

    let mut assets = release_assets(staging.path());
    assets.push((
        "artifacts-index.json".to_string(),
        br#"{"schema_version":"1","artifacts":[]}"#.to_vec(),
    ));
    stub.release(ANA, "v3", assets);
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-IDX-002",
        &[&store],
    );
    assert_contains(
        &stderr,
        &[
            "github:ana/artifacts release v3 is a schema 1 index",
            "this is a schema 1 artifact index",
        ],
    );

    let index = String::from_utf8(mur_index(staging.path(), ANA, "v4", None)).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&index).unwrap();
    value["artifacts"][0]["versions"][0]
        .as_object_mut()
        .unwrap()
        .remove("assets");
    let mut assets = release_assets(staging.path());
    assets.push((
        "artifacts-index.json".to_string(),
        serde_json::to_vec(&value).unwrap(),
    ));
    stub.release(ANA, "v4", assets);
    let stderr = refused(
        install(
            home.path(),
            cwd.path(),
            &stub,
            &["-g", "ana/tool-git@1.0.0"],
        ),
        "E-IDX-002",
        &[&store],
    );
    assert_contains(
        &stderr,
        &["github:ana/artifacts release v4", "assets", "--release v4"],
    );
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

// ─── S9: platforms ───────────────────────────────────────────────────────────

#[test]
fn a_platform_payload_is_installed_by_its_exact_file_or_refused() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    let dir = staging.path();
    native_tool_zip(dir, "tool-n", "0.1.0", "darwin-aarch64");
    native_tool_zip(dir, "tool-n", "0.1.0", "linux-x86_64");
    native_tool_zip(dir, "tool-d", "0.1.0", other_platform());
    stub.repository(ANA, 1111);
    publish_dir(&stub, ANA, "v1", dir);

    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);
    let store = global_store(home.path());
    let host = current_platform();

    if ["darwin-aarch64", "linux-x86_64"].contains(&host) {
        install(home.path(), cwd.path(), &stub, &["-g", "ana/tool-n@0.1.0"])
            .assert()
            .success();
        let file = format!("tool-n-0.1.0-{host}.mur.zip");
        assert_eq!(
            stored(home.path(), "ana/tool-n", "0.1.0", &file),
            fs::read(dir.join(&file)).unwrap()
        );
    } else {
        let stderr = refused(
            install(home.path(), cwd.path(), &stub, &["-g", "ana/tool-n@0.1.0"]),
            "E-REG-001",
            &[&store],
        );
        assert_contains(&stderr, &[host, "darwin-aarch64, linux-x86_64"]);
    }

    stub.clear_requests();
    let stderr = refused(
        install(home.path(), cwd.path(), &stub, &["-g", "ana/tool-d@0.1.0"]),
        "E-REG-001",
        &[&store],
    );
    assert_contains(&stderr, &[host, other_platform()]);
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}

#[test]
fn all_platforms_installs_each_published_file_and_warns_for_the_rest() {
    let stub = MockGitHub::start();
    let staging = tempfile::tempdir().unwrap();
    let dir = staging.path();
    native_tool_zip(dir, "tool-n", "0.1.0", "darwin-aarch64");
    native_tool_zip(dir, "tool-n", "0.1.0", "linux-x86_64");
    native_tool_zip(dir, "tool-d", "0.1.0", "darwin-aarch64");
    stub.repository(ANA, 1111);
    publish_dir(&stub, ANA, "v1", dir);

    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(home.path(), ANA_CONFIG);

    install(
        home.path(),
        cwd.path(),
        &stub,
        &["--all-platforms", "ana/tool-n@0.1.0"],
    )
    .assert()
    .success();
    for platform in ["darwin-aarch64", "linux-x86_64"] {
        let file = format!("tool-n-0.1.0-{platform}.mur.zip");
        assert_eq!(
            stored(home.path(), "ana/tool-n", "0.1.0", &file),
            fs::read(dir.join(&file)).unwrap(),
            "{platform}"
        );
    }

    let assert = install(
        home.path(),
        cwd.path(),
        &stub,
        &["--all-platforms", "ana/tool-d@0.1.0"],
    )
    .assert()
    .success();
    assert_contains(
        &stderr_of(&assert),
        &["warning: could not install ana/tool-d@0.1.0 for linux-x86_64"],
    );
    assert_eq!(
        files_under(&global_store(home.path()).join("ana/tool-d")),
        [
            "0.1.0/tool-d-0.1.0-darwin-aarch64.meta.json",
            "0.1.0/tool-d-0.1.0-darwin-aarch64.mur.zip",
            "0.1.0/tool-d-0.1.0-darwin-aarch64.sha256",
        ]
    );
}

// ─── S11: the github: form ───────────────────────────────────────────────────

#[test]
fn a_github_release_install_records_and_checks_the_owner() {
    let stub = MockGitHub::start();
    let _staging = ana_artifacts(&stub);
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_project(project.path(), &[]);

    install(
        home.path(),
        project.path(),
        &stub,
        &["github:ana/artifacts@v2"],
    )
    .assert()
    .success();
    let lock_path = project.path().join("murmur.lock");
    let lock = read_lockfile(&lock_path).unwrap();
    let names: Vec<&str> = lock.artifacts.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["ana/tool-git-validate", "ana/tool-git"]);
    assert!(lock.artifacts.iter().all(|e| e.owner_id == Some(1111)));
    let project_store = project.path().join(".murmur").join("artifacts");
    assert!(project_store
        .join("ana/tool-git/1.0.0/tool-git-1.0.0.mur.zip")
        .exists());

    stub.set_owner_id(ANA, 2222);
    stub.clear_requests();
    let lock_before = fs::read(&lock_path).unwrap();
    let stderr = refused(
        install(
            home.path(),
            project.path(),
            &stub,
            &["github:ana/artifacts@v2"],
        ),
        "E-REG-010",
        &[&project_store],
    );
    assert_contains(&stderr, &["1111", "2222"]);
    assert_eq!(fs::read(&lock_path).unwrap(), lock_before);
    assert_eq!(stub.paths(), ["/repos/ana/artifacts".to_string()]);

    stub.clear_requests();
    refused(
        install(
            home.path(),
            project.path(),
            &stub,
            &["-g", "github:murmur/x@v1"],
        ),
        "E-REG-007",
        &[&global_store(home.path())],
    );
    assert!(stub.paths().is_empty(), "{:?}", stub.paths());
}
