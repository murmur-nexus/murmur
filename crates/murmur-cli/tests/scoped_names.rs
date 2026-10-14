//! Every resolvable artifact is named `scope/name`, everywhere it is named for resolution, and the
//! model only ever sees the bare name.
//!
//! Each case runs the real `mur` binary under a scratch `HOME`. A command that must not reach a
//! source runs with `MUR_GITHUB_API_BASE` pointing at a listener that never accepts, so a lookup
//! shows up as a completed connection in its backlog.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::ErrorKind,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::{assert::Assert, Command};
use common::github_release::MockRelease;
use murmur_artifact::{
    read_lockfile, sha256_hex, write_lockfile_atomic, ArtifactMeta, ArtifactName, LocalRegistry,
    LockOrigin, LockedArtifact, LockedSha256, MurmurLock, Registry, RuntimeType, LOCK_VERSION,
};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur/driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const LOCK_V2_REFUSAL: &str =
    "unsupported lock_version 2 (expected 3) \u{2014} delete murmur.lock \
                               and run `mur install` to regenerate it";

// ── Harness ───────────────────────────────────────────────────────────────────

/// A listener that is bound and never accepted, and the `MUR_GITHUB_API_BASE` that reaches it.
struct SilentSource {
    listener: TcpListener,
    api_base: String,
}

impl SilentSource {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        Self { listener, api_base }
    }

    /// Whether anything connected: a connection the kernel completed waits in the backlog.
    fn was_contacted(&self) -> bool {
        self.listener.set_nonblocking(true).unwrap();
        match self.listener.accept() {
            Ok(_) => true,
            Err(err) if err.kind() == ErrorKind::WouldBlock => false,
            Err(err) => panic!("accept on the silent listener failed: {err}"),
        }
    }
}

/// `mur` under `home`, from `cwd`, with every GitHub lookup sent to `api_base`.
fn mur(home: &TempDir, cwd: &Path, api_base: &str) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(120));
    cmd
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

/// An otherwise valid agent capsule declaring `artifacts` (YAML list items, already indented)
/// beside the driver, plus `blocks` at the top level.
fn agent_manifest(project: &Path, artifacts: &str, blocks: &str) -> PathBuf {
    let yaml = format!(
        "name: scoped-capsule\nversion: 0.1.0\n{blocks}artifacts:\n{artifacts}  - name: {DRIVER}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
         http://127.0.0.1:1\n      api_key: test-key\ninference:\n  transport: http\n  model: \
         test-model\n  driver:\n    artifact: {DRIVER}\n"
    );
    let path = project.join("murmur.yaml");
    fs::write(&path, yaml).unwrap();
    path
}

fn tool_entry(name: &str) -> String {
    format!("  - name: {name}\n    version: 0.6.0\n    runtime: tool\n")
}

/// Whether `project` holds anything besides `murmur.yaml` (and the inputs a case wrote).
fn created_anything(project: &Path) -> bool {
    project.join("workdir").exists()
        || project.join(".murmur").exists()
        || project.join("murmur.lock").exists()
}

/// File `zip` into the store at `root` under `name` through the Rust store API.
fn store_as(root: &Path, name: &str, version: &str, zip: &Path) {
    let bytes = fs::read(zip).unwrap();
    LocalRegistry::new(root)
        .store_installed_overwrite(
            ArtifactMeta {
                name: ArtifactName::parse(name).unwrap(),
                version: version.to_string(),
                runtime: RuntimeType::Wasm,
                artifact_runtime: "tool".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &bytes,
            &sha256_hex(&bytes),
        )
        .unwrap();
}

fn lock_entry(name: &str, version: &str, zip: &Path) -> LockedArtifact {
    LockedArtifact {
        name: name.to_string(),
        resolved_version: version.to_string(),
        sha256: LockedSha256::any(sha256_hex(&fs::read(zip).unwrap())),
        origin: LockOrigin::Operator,
        owner_id: None,
    }
}

fn echo_tool_wasm() -> PathBuf {
    common::fixture_path("run/components/echo-tool.wasm")
}

fn driver_wasm() -> PathBuf {
    common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm")
}

/// A `.mur.zip` packed as the WASM tool `echo-tool@0.0.1`, under `dir`.
fn echo_tool_zip(dir: &Path) -> PathBuf {
    common::create_tool_artifact(dir, "echo-tool", "0.0.1", &echo_tool_wasm())
}

// ── The manifest refuses a bare name ──────────────────────────────────────────

#[test]
fn a_bare_artifact_name_in_murmur_yaml_is_refused_with_the_scoped_forms() {
    for bare in ["tool-editor", "murmur-tool-editor"] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        agent_manifest(project.path(), &tool_entry(bare), "");
        let source = SilentSource::new();

        for args in [vec!["run", "--task", "x"], vec!["install"], vec!["doctor"]] {
            let assert = mur(&home, project.path(), &source.api_base)
                .args(&args)
                .assert()
                .failure();
            let stderr = stderr_of(&assert);
            assert!(stderr.contains("error[E-MAN-003]"), "{args:?}: {stderr}");
            assert!(stderr.contains("artifacts[0].name"), "{args:?}: {stderr}");
            assert!(stderr.contains("hint:"), "{args:?}: {stderr}");
            assert!(stderr.contains("murmur/tool-editor"), "{args:?}: {stderr}");
            assert!(
                stderr.contains(&format!("local/{bare}")),
                "{args:?}: {stderr}"
            );
        }
        assert!(
            !created_anything(project.path()),
            "a refusal creates nothing"
        );
        assert!(!source.was_contacted());
    }
}

#[test]
fn a_scoped_name_passes_parsing_and_fails_on_the_missing_artifact() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    agent_manifest(project.path(), &tool_entry("murmur/tool-editor"), "");
    let source = SilentSource::new();

    let assert = mur(&home, project.path(), &source.api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(!stderr.contains("E-MAN-003"), "{stderr}");
    assert!(stderr.contains("error[E-RUN-008]"), "{stderr}");
    assert!(stderr.contains("murmur/tool-editor@0.6.0"), "{stderr}");
}

// ── The other surfaces ────────────────────────────────────────────────────────

#[test]
fn install_entries_and_spawn_allow_refuse_bare_names() {
    let cases = [
        (
            "capabilities:\n  install:\n    tool: [tool-*]\n",
            vec!["capabilities.install.tool[0]", "murmur/tool-*"],
        ),
        (
            "capabilities:\n  spawn:\n    allow: [worker]\n",
            vec!["capabilities.spawn.allow[0]", "local/worker"],
        ),
        (
            "capabilities:\n  spawn:\n    allow: [local/worker, ana/worker]\n",
            vec!["local/worker", "ana/worker", "'worker'"],
        ),
    ];
    for (blocks, expected) in cases {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        agent_manifest(project.path(), "", blocks);
        let assert = mur(&home, project.path(), &SilentSource::new().api_base)
            .args(["run", "--task", "x"])
            .assert()
            .failure();
        let stderr = stderr_of(&assert);
        assert!(stderr.contains("error[E-MAN-003]"), "{blocks}: {stderr}");
        for part in expected {
            assert!(
                stderr.contains(part),
                "{blocks}: missing {part:?} in {stderr}"
            );
        }
    }
}

#[test]
fn a_source_skill_named_with_a_scope_is_told_to_drop_it() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("skills/style")).unwrap();
    fs::write(project.path().join("skills/style/skill.md"), "# Style\n").unwrap();
    agent_manifest(
        project.path(),
        "  - name: ana/style\n    runtime: skill\n    source: ./skills/style\n",
        "",
    );
    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-MAN-003]"), "{stderr}");
    assert!(
        stderr.contains("drop the scope: name it 'style'"),
        "{stderr}"
    );
}

#[test]
fn a_driver_reference_by_bare_name_is_refused_naming_the_declared_entry() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = agent_manifest(project.path(), "", "");
    let yaml = fs::read_to_string(&path)
        .unwrap()
        .replace(&format!("artifact: {DRIVER}"), "artifact: driver-anthropic");
    fs::write(&path, yaml).unwrap();

    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(
        stderr.contains("artifact 'driver-anthropic' is not declared in artifacts:"),
        "{stderr}"
    );
    assert!(stderr.contains(DRIVER), "{stderr}");
}

#[test]
fn a_roster_member_with_a_bare_capsule_is_refused_with_the_scoped_forms() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    agent_manifest(project.path(), "", "");
    fs::write(
        project.path().join("roster.yaml"),
        "roster_version: 1\nmembers:\n  - name: lead\n    capsule: crew-lead\n    version: 0.1.0\n    \
         entry: true\n",
    )
    .unwrap();

    for args in [vec!["run", "--roster", ".", "--task", "x"], vec!["doctor"]] {
        let assert = mur(&home, project.path(), &SilentSource::new().api_base)
            .args(&args)
            .assert()
            .failure();
        let stderr = stderr_of(&assert);
        assert!(stderr.contains("E-ROS-001"), "{args:?}: {stderr}");
        assert!(stderr.contains("members[0].capsule"), "{args:?}: {stderr}");
        assert!(stderr.contains("local/crew-lead"), "{args:?}: {stderr}");
        assert!(stderr.contains("murmur/crew-lead"), "{args:?}: {stderr}");
    }
}

#[test]
fn a_command_line_name_that_is_not_scope_name_is_e_reg_007() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let source = SilentSource::new();

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["install", "-g", "tool-editor@0.6.0"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-REG-007]"), "{stderr}");
    assert!(stderr.contains("murmur/tool-editor"), "{stderr}");

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["install", "-g", "tool-editor"])
        .assert()
        .failure();
    assert!(stderr_of(&assert).contains("error[E-REG-007]"));

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["install", "-g", "a/b/c@1.0.0"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-REG-007]"), "{stderr}");
    assert!(stderr.contains("exactly one '/'"), "{stderr}");

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["run", "--capsule", "worker", "--capsule-version", "0.1.0"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-REG-007]"), "{stderr}");
    assert!(stderr.contains("local/worker"), "{stderr}");

    assert!(!source.was_contacted());
    assert!(!home.path().join(".murmur/artifacts").exists());
}

// ── Locally built artifacts are `local/` ──────────────────────────────────────

#[test]
fn a_local_file_installs_as_local_and_locks_lock_version_3() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    agent_manifest(project.path(), "", "");
    let zip = echo_tool_zip(artifacts.path());
    let source = SilentSource::new();

    let assert = mur(&home, project.path(), &source.api_base)
        .args(["install", "--no-precompile", zip.to_str().unwrap()])
        .assert()
        .success();
    assert!(
        stdout_of(&assert).contains("Installed local/echo-tool@0.0.1"),
        "{}",
        stdout_of(&assert)
    );

    let dir = project
        .path()
        .join(".murmur/artifacts/local/echo-tool/0.0.1");
    for file in [
        "echo-tool-0.0.1.mur.zip",
        "echo-tool-0.0.1.sha256",
        "echo-tool-0.0.1.meta.json",
    ] {
        assert!(
            dir.join(file).exists(),
            "{file} is filed under the local scope"
        );
    }
    let meta: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("echo-tool-0.0.1.meta.json")).unwrap())
            .unwrap();
    assert_eq!(meta["meta"]["name"], "local/echo-tool");

    let raw = fs::read_to_string(project.path().join("murmur.lock")).unwrap();
    assert!(raw.contains("lock_version: 3"), "{raw}");
    assert!(!raw.contains("owner_id"), "{raw}");
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock.artifact_for("local/echo-tool").unwrap();
    assert_eq!(entry.origin, LockOrigin::Operator);
    assert_eq!(entry.owner_id, None);

    let assert = mur(&home, project.path(), &source.api_base)
        .args(["list"])
        .assert()
        .success();
    assert!(stdout_of(&assert).contains("local/echo-tool"));
    assert!(!source.was_contacted());
}

#[test]
fn publish_files_a_local_artifact_in_the_global_store() {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let zip = echo_tool_zip(artifacts.path());

    let assert = mur(&home, artifacts.path(), &SilentSource::new().api_base)
        .args(["publish", "echo-tool-0.0.1.mur.zip"])
        .assert()
        .success();
    assert!(
        stdout_of(&assert).contains("Published local/echo-tool@0.0.1"),
        "{}",
        stdout_of(&assert)
    );
    assert!(zip.exists());
    assert!(home
        .path()
        .join(".murmur/artifacts/local/echo-tool/0.0.1/echo-tool-0.0.1.mur.zip")
        .exists());
}

#[test]
fn a_local_store_miss_names_the_file_install_and_asks_no_source() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let source = SilentSource::new();

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["install", "-g", "local/absent@1.0.0"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-REG-001]"), "{stderr}");
    assert!(stderr.contains("local/absent@1.0.0"), "{stderr}");
    assert!(
        stderr.contains("mur install ./absent-1.0.0.mur.zip"),
        "{stderr}"
    );

    let assert = mur(&home, cwd.path(), &source.api_base)
        .args(["install", "-g", "--all-platforms", "local/absent@1.0.0"])
        .assert()
        .failure();
    assert!(stderr_of(&assert).contains("error[E-REG-001]"));

    for reserved in ["github:local/repo@v1", "github:murmur/repo@v1"] {
        let assert = mur(&home, cwd.path(), &source.api_base)
            .args(["install", "-g", reserved])
            .assert()
            .failure();
        let stderr = stderr_of(&assert);
        assert!(stderr.contains("reserved scope"), "{reserved}: {stderr}");
    }

    assert!(
        !source.was_contacted(),
        "no source is asked about the local scope"
    );
}

/// A capsule declaring only `artifacts` (YAML list items), so `mur install` asks about nothing
/// else.
fn plain_manifest(project: &Path, artifacts: &str) {
    fs::write(
        project.join("murmur.yaml"),
        format!("name: plain-capsule\nversion: 0.1.0\nartifacts:\n{artifacts}"),
    )
    .unwrap();
}

#[test]
fn a_manifest_install_of_a_missing_local_artifact_asks_no_source() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    plain_manifest(project.path(), &tool_entry("local/absent"));
    let source = SilentSource::new();

    let assert = mur(&home, project.path(), &source.api_base)
        .args(["install"])
        .assert()
        .failure();
    let output = format!("{}{}", stdout_of(&assert), stderr_of(&assert));
    assert!(output.contains("E-REG-001"), "{output}");
    assert!(output.contains(".mur.zip"), "{output}");
    assert!(!source.was_contacted());
}

// ── Sources ───────────────────────────────────────────────────────────────────

/// A `.mur.zip` packed as the WASM tool `packed@version`, under `dir`, named `file_name`.
fn packed_tool(dir: &Path, packed: &str, version: &str, file_name: &str) -> Vec<u8> {
    let zip = common::create_tool_artifact(dir, packed, version, &echo_tool_wasm());
    let renamed = dir.join(file_name);
    fs::rename(&zip, &renamed).unwrap();
    fs::read(renamed).unwrap()
}

#[test]
fn a_scoped_install_asks_the_sources_for_the_bare_name_and_stores_the_full_one() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    agent_manifest(project.path(), "", "");
    let bytes = packed_tool(staging.path(), "tool-x", "1.0.0", "tool-x-1.0.0.mur.zip");
    let release = MockRelease::start("v1.0.0", vec![("tool-x-1.0.0.mur.zip".to_string(), bytes)]);

    mur(&home, project.path(), &release.api_base)
        .args(["install", "--no-precompile", "ana/tool-x@1.0.0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Installed ana/tool-x@1.0.0"));

    assert!(project
        .path()
        .join(".murmur/artifacts/ana/tool-x/1.0.0/tool-x-1.0.0.mur.zip")
        .exists());
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    assert!(lock.artifact_for("ana/tool-x").is_some());
    // The release lists one asset, `tool-x-1.0.0.mur.zip`, so fetching it means the source was
    // asked for the bare name.
    assert!(
        release
            .paths()
            .iter()
            .any(|path| path.ends_with("/releases/assets/1")),
        "the asset named by the bare name was fetched: {:?}",
        release.paths()
    );
}

#[test]
fn a_github_release_is_stored_under_the_scope_of_its_repository() {
    for (owner, repo, scope) in [
        ("ana", "repo", "ana"),
        ("murmur-nexus", "default-artifacts", "murmur"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let first = packed_tool(staging.path(), "tool-x", "1.0.0", "tool-x-1.0.0.mur.zip");
        let second = packed_tool(staging.path(), "tool-y", "2.0.0", "tool-y-2.0.0.mur.zip");
        let release = MockRelease::start_tagged(
            "v1",
            vec![
                ("tool-x-1.0.0.mur.zip".to_string(), first),
                ("tool-y-2.0.0.mur.zip".to_string(), second),
            ],
        );

        mur(&home, cwd.path(), &release.api_base)
            .args([
                "install",
                "-g",
                "--no-precompile",
                &format!("github:{owner}/{repo}@v1"),
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "Installed {scope}/tool-x@1.0.0"
            )))
            .stdout(predicate::str::contains(format!(
                "Installed {scope}/tool-y@2.0.0"
            )));

        let store = home.path().join(".murmur/artifacts");
        assert!(store
            .join(format!("{scope}/tool-x/1.0.0/tool-x-1.0.0.mur.zip"))
            .exists());
        assert!(store
            .join(format!("{scope}/tool-y/2.0.0/tool-y-2.0.0.mur.zip"))
            .exists());
    }
}

#[test]
fn an_asset_packed_under_another_name_is_refused_and_stores_nothing() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let bytes = packed_tool(staging.path(), "tool-y", "1.0.0", "tool-x-1.0.0.mur.zip");
    let release = MockRelease::start("v1.0.0", vec![("tool-x-1.0.0.mur.zip".to_string(), bytes)]);

    let assert = mur(&home, cwd.path(), &release.api_base)
        .args(["install", "-g", "--no-precompile", "ana/tool-x@1.0.0"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-REG-002]"), "{stderr}");
    assert!(stderr.contains("ana/tool-x"), "{stderr}");
    assert!(stderr.contains("'tool-y'"), "{stderr}");
    assert!(
        LocalRegistry::new(home.path().join(".murmur/artifacts"))
            .list_index()
            .unwrap()
            .is_empty(),
        "nothing is stored"
    );
}

// ── The lock ──────────────────────────────────────────────────────────────────

/// An agent project declaring `local/echo-tool`, installed from its `.mur.zip`, beside the driver
/// already in its store, so nothing it declares is asked of a source.
fn project_with_echo_tool(home: &TempDir) -> (TempDir, TempDir) {
    let project = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    agent_manifest(
        project.path(),
        &tool_entry_at("local/echo-tool", "0.0.1"),
        "",
    );
    let driver =
        common::create_driver_artifact(artifacts.path(), DRIVER, DRIVER_VERSION, &driver_wasm());
    store_as(
        &project.path().join(".murmur/artifacts"),
        DRIVER,
        DRIVER_VERSION,
        &driver,
    );
    let zip = echo_tool_zip(artifacts.path());
    mur(home, project.path(), &SilentSource::new().api_base)
        .args(["install", "--no-precompile", zip.to_str().unwrap()])
        .assert()
        .success();
    (project, artifacts)
}

fn tool_entry_at(name: &str, version: &str) -> String {
    format!("  - name: {name}\n    version: {version}\n    runtime: tool\n")
}

#[test]
fn a_version_2_lock_is_refused_for_its_version_by_run_install_and_doctor() {
    let home = tempfile::tempdir().unwrap();
    let (project, _artifacts) = project_with_echo_tool(&home);
    let lock_path = project.path().join("murmur.lock");
    let v2 = fs::read_to_string(&lock_path)
        .unwrap()
        .replace("lock_version: 3", "lock_version: 2")
        .replace("name: local/echo-tool", "name: echo-tool");
    fs::write(&lock_path, v2).unwrap();

    for args in [vec!["run", "--task", "x"], vec!["install"], vec!["doctor"]] {
        let assert = mur(&home, project.path(), &SilentSource::new().api_base)
            .args(&args)
            .assert()
            .failure();
        let output = format!("{}{}", stdout_of(&assert), stderr_of(&assert));
        assert!(output.contains(LOCK_V2_REFUSAL), "{args:?}: {output}");
        assert!(!output.contains("<scope>/<name>"), "{args:?}: {output}");
    }
}

#[test]
fn a_run_on_a_version_2_lock_is_e_run_003() {
    let home = tempfile::tempdir().unwrap();
    let (project, _artifacts) = project_with_echo_tool(&home);
    let lock_path = project.path().join("murmur.lock");
    let v2 = fs::read_to_string(&lock_path)
        .unwrap()
        .replace("lock_version: 3", "lock_version: 2");
    fs::write(&lock_path, v2).unwrap();

    mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-RUN-003]"));
}

#[test]
fn a_version_3_lock_with_a_bare_name_is_refused_naming_it() {
    let home = tempfile::tempdir().unwrap();
    let (project, _artifacts) = project_with_echo_tool(&home);
    let lock_path = project.path().join("murmur.lock");
    let bare = fs::read_to_string(&lock_path)
        .unwrap()
        .replace("name: local/echo-tool", "name: echo-tool");
    fs::write(&lock_path, bare).unwrap();

    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("'echo-tool'"), "{stderr}");
    assert!(
        stderr.contains("delete murmur.lock and run `mur install` to regenerate it"),
        "{stderr}"
    );
}

#[test]
fn an_owner_id_survives_a_lock_rewrite_and_a_new_entry_carries_none() {
    let home = tempfile::tempdir().unwrap();
    let (project, artifacts) = project_with_echo_tool(&home);
    let lock_path = project.path().join("murmur.lock");
    let mut lock = read_lockfile(&lock_path).unwrap();
    lock.artifacts[0].owner_id = Some(12345);
    write_lockfile_atomic(&lock_path, &lock).unwrap();
    assert!(read_lockfile(&lock_path).is_ok());

    let second = common::create_skill_artifact(artifacts.path(), "style", "0.1.0", "# Style\n");
    mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["install", "--no-precompile", second.to_str().unwrap()])
        .assert()
        .success();
    mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["install", "--no-precompile"])
        .assert()
        .success();

    let raw = fs::read_to_string(&lock_path).unwrap();
    assert_eq!(raw.matches("owner_id").count(), 1, "{raw}");
    let lock = read_lockfile(&lock_path).unwrap();
    assert_eq!(
        lock.artifact_for("local/echo-tool").unwrap().owner_id,
        Some(12345)
    );
    assert_eq!(lock.artifact_for("local/style").unwrap().owner_id, None);

    fs::remove_file(&lock_path).unwrap();
    mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["install", "--no-precompile"])
        .assert()
        .success();
    let lock = read_lockfile(&lock_path).unwrap();
    assert_eq!(lock.lock_version, LOCK_VERSION);
    assert!(lock.artifact_for("local/echo-tool").is_some());
}

// ── Staging: tool names are unique, and the model sees only them ──────────────

/// A project whose store holds the driver and, for each `(full name, description)`, the WASM
/// echo tool packed as `tool-editor`, with a v3 lock entry for every one of them.
fn staging_project(
    tools: &[(&str, &str)],
    declared: &[&str],
    endpoint: &str,
) -> (TempDir, PathBuf) {
    let project = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let store = project.path().join(".murmur/artifacts");

    let driver =
        common::create_driver_artifact(artifacts.path(), DRIVER, DRIVER_VERSION, &driver_wasm());
    store_as(&store, DRIVER, DRIVER_VERSION, &driver);
    let mut entries = vec![lock_entry(DRIVER, DRIVER_VERSION, &driver)];

    for (index, (name, description)) in tools.iter().enumerate() {
        let dir = artifacts.path().join(index.to_string());
        fs::create_dir_all(&dir).unwrap();
        let zip = common::create_tool_artifact_with_auth(
            &dir,
            name,
            "0.6.0",
            &echo_tool_wasm(),
            &format!("description: {description}\n"),
        );
        store_as(&store, name, "0.6.0", &zip);
        entries.push(lock_entry(name, "0.6.0", &zip));
    }
    write_lockfile_atomic(
        &project.path().join("murmur.lock"),
        &MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: entries,
        },
    )
    .unwrap();

    let artifacts_yaml: String = declared.iter().map(|name| tool_entry(name)).collect();
    let path = project.path().join("murmur.yaml");
    fs::write(
        &path,
        format!(
            "name: scoped-capsule\nversion: 0.1.0\nartifacts:\n{artifacts_yaml}  - name: {DRIVER}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
             api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  \
             transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n"
        ),
    )
    .unwrap();
    (project, path)
}

#[test]
fn two_scopes_serving_one_tool_name_are_refused_naming_both() {
    let home = tempfile::tempdir().unwrap();
    let declared = ["murmur/tool-editor", "ana/tool-editor"];
    let (project, _manifest) = staging_project(
        &[
            ("murmur/tool-editor", "the murmur editor"),
            ("ana/tool-editor", "ana's editor"),
        ],
        &declared,
        "http://127.0.0.1:1",
    );

    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-CAP-020]"), "{stderr}");
    for part in ["murmur/tool-editor", "ana/tool-editor", "'tool-editor'"] {
        assert!(stderr.contains(part), "missing {part}: {stderr}");
    }
    assert!(
        !project.path().join("workdir").exists(),
        "nothing is staged"
    );

    // With both stores empty the refusal is the same: nothing is resolved first.
    let empty = tempfile::tempdir().unwrap();
    agent_manifest(
        empty.path(),
        &declared
            .iter()
            .map(|name| tool_entry(name))
            .collect::<String>(),
        "",
    );
    let assert = mur(&home, empty.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-CAP-020]"), "{stderr}");
    assert!(!stderr.contains("E-REG-001"), "{stderr}");
    assert!(!empty.path().join("workdir").exists());
}

#[test]
fn a_source_skill_and_a_resolved_artifact_sharing_a_tool_name_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("skills/editor")).unwrap();
    fs::write(project.path().join("skills/editor/skill.md"), "# Editor\n").unwrap();
    agent_manifest(
        project.path(),
        &format!(
            "  - name: tool-editor\n    runtime: skill\n    source: ./skills/editor\n{}",
            tool_entry("murmur/tool-editor")
        ),
        "",
    );
    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-CAP-020]"), "{stderr}");
    assert!(stderr.contains("murmur/tool-editor"), "{stderr}");
}

#[test]
fn a_reserved_tool_name_is_refused_on_the_bare_part() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    agent_manifest(project.path(), &tool_entry("murmur/delegate-task"), "");
    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["run", "--task", "x"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-CAP-013]"), "{stderr}");
    assert!(stderr.contains("'delegate-task'"), "{stderr}");
}

#[test]
fn the_model_is_offered_the_bare_tool_name_and_never_a_scope() {
    let home = tempfile::tempdir().unwrap();
    let server = common::ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let (project, manifest) = staging_project(
        &[("murmur/tool-editor", "edits files")],
        &["murmur/tool-editor"],
        &server.endpoint,
    );
    let workdir = project.path().join("pinned");
    fs::create_dir_all(&workdir).unwrap();

    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--workdir",
            workdir.to_str().unwrap(),
            "--task",
            "edit",
        ])
        .assert()
        .success();
    let stdout = stdout_of(&assert);
    let session = stdout
        .split("session: ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .unwrap_or_else(|| panic!("no session id in stdout: {stdout}"))
        .trim()
        .to_string();

    let requests = server.requests();
    let tools = requests[0]["tools"].as_array().expect("a tool array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"tool-editor"), "{names:?}");
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or_default();
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(!name.contains('/'), "a tool name carries no scope: {name}");
        assert!(!description.contains("murmur/"), "{description}");
    }
    assert!(workdir
        .join(".murmur")
        .join(&session)
        .join("tools/tool-editor/murmur.yaml")
        .exists());
}

// ── Doctor and the old store layout ───────────────────────────────────────────

#[test]
fn doctor_names_an_old_layout_store_directory_as_safe_to_delete() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: plain\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();
    let zip = echo_tool_zip(artifacts.path());
    mur(&home, artifacts.path(), &SilentSource::new().api_base)
        .args(["publish", zip.to_str().unwrap()])
        .assert()
        .success();

    let without = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["doctor"])
        .output()
        .unwrap();

    let old = home.path().join(".murmur/artifacts/murmur-tool-git/0.2.0");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("murmur-tool-git-0.2.0.mur.zip"), b"old").unwrap();
    fs::write(old.join("murmur-tool-git-0.2.0.sha256"), sha256_hex(b"old")).unwrap();

    let with = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["doctor"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&with.stdout);
    assert!(stdout.contains("Artifact stores"), "{stdout}");
    assert!(
        stdout.contains("murmur-tool-git: old layout, nothing reads it, safe to delete"),
        "{stdout}"
    );
    assert!(stdout.contains("global store"), "{stdout}");
    assert_eq!(with.status.code(), without.status.code());

    let assert = mur(&home, project.path(), &SilentSource::new().api_base)
        .args(["list", "-g"])
        .assert()
        .success();
    let listed = stdout_of(&assert);
    assert!(listed.contains("local/echo-tool"), "{listed}");
    assert!(!listed.contains("murmur-tool-git"), "{listed}");
}

// ── `mur build` packs the bare name ───────────────────────────────────────────

#[test]
fn build_refuses_a_scoped_name_and_packs_a_bare_one() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    fs::copy(echo_tool_wasm(), source.path().join("tool.wasm")).unwrap();
    let manifest = |name: &str| {
        fs::write(
            source.path().join("murmur.yaml"),
            format!(
                "name: {name}\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - tool.wasm\n"
            ),
        )
        .unwrap();
    };

    manifest("ana/tool-x");
    let assert = mur(&home, source.path(), &SilentSource::new().api_base)
        .args(["build", "."])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("ana/tool-x"), "{stderr}");
    assert!(
        stderr.contains("the scope belongs to the source the artifact is published from"),
        "{stderr}"
    );

    manifest("tool-x");
    mur(&home, source.path(), &SilentSource::new().api_base)
        .args(["build", "."])
        .assert()
        .success();
    let built = source.path().join("tool-x-0.1.0.mur.zip");
    let packed = murmur_artifact::load_manifest_from_artifact(&built).unwrap();
    assert_eq!(packed.name, "tool-x");

    let skill = tempfile::tempdir().unwrap();
    fs::write(skill.path().join("SKILL.md"), "# A skill\n").unwrap();
    let assert = mur(&home, skill.path(), &SilentSource::new().api_base)
        .args(["build", "--skill", "ana/x", skill.path().to_str().unwrap()])
        .assert()
        .failure();
    assert!(stderr_of(&assert).contains("the scope belongs to the source"));
}
