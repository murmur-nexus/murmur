//! An artifact whose own `murmur.yaml` declares `implementation: native` or `execution: native` is
//! refused wherever `mur` meets it: `mur build` with `E-BLD-007`, `mur publish` and every
//! `mur install` form with `E-REG-012`, a launch and `mur doctor` with `E-RUN-061`, and a runtime
//! pull with `E-RUN-061` in the tool result.
//!
//! Every case drives the `mur` binary built from this tree. `mur build` refuses a native manifest,
//! so the native payload is a zip written here: S1's manifest beside a dummy `bin/nat-tool`. A
//! store an older `mur` left is seeded file by file, the way that `mur` filed a native install.
//! GitHub is the loopback stub `MockGitHub`, reached through `MUR_GITHUB_API_BASE`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::{assert::Assert, Command};
use common::github_release::{mur_index, release_assets, wasm_tool_zip, MockGitHub};
use murmur_artifact::{read_lockfile, sha256_hex};
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

const NAT: &str = "nat-tool";
const NAT_VERSION: &str = "0.1.0";
const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const ANA: &str = "ana/artifacts";
const ANA_OWNER_ID: u64 = 4242;
const ANA_CONFIG: &str = "registry:\n  sources:\n    - repo: ana/artifacts\n";

// ── harness ──────────────────────────────────────────────────────────────────

/// `mur` under `home`, run from `cwd`, with no GitHub token in reach and every GitHub request sent
/// to `stub` when given.
fn mur(home: &Path, cwd: &Path, stub: Option<&MockGitHub>) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(180));
    if let Some(stub) = stub {
        command.env("MUR_GITHUB_API_BASE", &stub.api_base);
    }
    command
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

/// The manifest S1 builds and S2 packs: a tool declaring `field: value`.
fn native_manifest(field: &str, value: &str) -> String {
    format!(
        "name: {NAT}\nversion: {NAT_VERSION}\nruntime: tool\n{field}: {value}\nrequires_files:\n  \
         - bin/{NAT}\n"
    )
}

/// `nat-tool-0.1.0.mur.zip`'s bytes as an older `mur build` packed them: the manifest declaring
/// `implementation: native` and a dummy `bin/nat-tool`.
fn native_zip() -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        zip.start_file("murmur.yaml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(native_manifest("implementation", "native").as_bytes())
            .unwrap();
        zip.start_file(format!("bin/{NAT}"), SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"#!/bin/sh\necho native\n").unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// The platform tag an older `mur` filed a native install for this host under.
fn host_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Every file under `dir`, relative to it; empty when `dir` does not exist.
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

/// How an older `mur` filed a native install of `local/nat-tool@0.1.0`.
#[derive(Clone, Copy, Debug)]
enum LegacyLayout {
    /// `nat-tool-0.1.0-<platform>.mur.zip` with a sidecar recording that platform.
    Tagged,
    /// `nat-tool-0.1.0.mur.zip` with a sidecar recording no platform.
    Generic,
}

/// Seeds the store at `store` with `local/nat-tool@0.1.0` as an older `mur` left it under
/// `layout`: the payload, its `.sha256` and a sidecar recording `runtime: native`. Returns the
/// payload's sha256.
fn seed_legacy_native(store: &Path, layout: LegacyLayout) -> String {
    let bytes = native_zip();
    let sha256 = sha256_hex(&bytes);
    let dir = store.join("local").join(NAT).join(NAT_VERSION);
    fs::create_dir_all(&dir).unwrap();
    let (stem, platforms) = match layout {
        LegacyLayout::Tagged => {
            let platform = host_platform();
            let (os, arch) = platform.split_once('-').unwrap();
            (
                format!("{NAT}-{NAT_VERSION}-{platform}"),
                json!([[os, arch]]),
            )
        }
        LegacyLayout::Generic => (format!("{NAT}-{NAT_VERSION}"), json!([])),
    };
    fs::write(dir.join(format!("{stem}.mur.zip")), &bytes).unwrap();
    fs::write(dir.join(format!("{stem}.sha256")), sha256.as_bytes()).unwrap();
    let sidecar = json!({
        "meta": {
            "name": format!("local/{NAT}"),
            "version": NAT_VERSION,
            "runtime": "native",
            "artifact_runtime": "tool",
            "platforms": platforms,
            "description": null,
            "tags": [],
        }
    });
    fs::write(
        dir.join(format!("{stem}.meta.json")),
        serde_json::to_string_pretty(&sidecar).unwrap(),
    )
    .unwrap();
    sha256
}

/// A capsule driven by the scripted driver at `endpoint`, declaring `artifacts` beside its driver
/// and `capabilities` lines beside its network grant.
fn capsule_yaml(endpoint: &str, artifacts: &str, capabilities: &str) -> String {
    format!(
        "name: nat-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\n{artifacts}capabilities:\n{capabilities}  network:\n    allow:\n      \
         - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    \
         artifact: {DRIVER_NAME}\n"
    )
}

/// The artifacts entry declaring `local/nat-tool@0.1.0`.
fn nat_entry() -> String {
    format!("  - name: local/{NAT}\n    version: {NAT_VERSION}\n    runtime: tool\n")
}

/// A home whose global store holds the scripted driver, and a project directory beside it.
struct Launch {
    home: TempDir,
    project: TempDir,
    _staging: TempDir,
    driver_sha256: String,
}

impl Launch {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let driver = common::create_driver_artifact(
            staging.path(),
            DRIVER_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&home, &driver).success();
        let driver_sha256 = sha256_hex(&fs::read(&driver).unwrap());
        Self {
            home,
            project,
            _staging: staging,
            driver_sha256,
        }
    }

    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn write_manifest(&self, yaml: &str) {
        fs::write(self.manifest(), yaml).unwrap();
    }

    fn lock_path(&self) -> PathBuf {
        self.project.path().join("murmur.lock")
    }

    /// Writes a version-3 `murmur.lock` pinning the driver and each `(name, version, sha256)`
    /// of `entries` under `sha256.any`.
    fn write_lock(&self, entries: &[(&str, &str, &str)]) {
        let mut yaml = format!(
            "lock_version: 3\nartifacts:\n- name: {DRIVER_NAME}\n  resolved_version: \
             {DRIVER_VERSION}\n  sha256:\n    any: {}\n  origin: operator\n",
            self.driver_sha256
        );
        for (name, version, sha256) in entries {
            yaml.push_str(&format!(
                "- name: {name}\n  resolved_version: {version}\n  sha256:\n    any: {sha256}\n  \
                 origin: operator\n"
            ));
        }
        fs::write(self.lock_path(), yaml).unwrap();
    }

    fn mur(&self) -> Command {
        mur(self.home.path(), self.project.path(), None)
    }

    fn run(&self) -> Assert {
        self.mur()
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "hi",
                "--verbose",
            ])
            .assert()
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── S1: mur build ────────────────────────────────────────────────────────────

/// A source directory holding `manifest` and a dummy `bin/nat-tool`.
fn native_source(manifest: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("murmur.yaml"), manifest).unwrap();
    fs::create_dir_all(dir.path().join("bin")).unwrap();
    fs::write(dir.path().join("bin").join(NAT), b"#!/bin/sh\n").unwrap();
    dir
}

fn built_zips(dir: &Path) -> Vec<String> {
    files_under(dir)
        .into_iter()
        .filter(|file| file.ends_with(".mur.zip"))
        .collect()
}

#[test]
fn s1_build_refuses_implementation_native_and_writes_no_zip() {
    let home = tempfile::tempdir().unwrap();
    let source = native_source(&native_manifest("implementation", "native"));

    let assert = mur(home.path(), source.path(), None)
        .arg("build")
        .assert()
        .failure();

    let stderr = stderr_of(&assert);
    println!("S1 implementation: native:\n{stderr}");
    assert!(stderr.contains("error[E-BLD-007]"), "{stderr}");
    assert!(
        stderr.contains("'nat-tool'") && stderr.contains("implementation: native"),
        "{stderr}"
    );
    assert!(built_zips(source.path()).is_empty());
    assert!(built_zips(home.path()).is_empty());
}

#[test]
fn s1_build_refuses_execution_native_naming_execution() {
    let home = tempfile::tempdir().unwrap();
    let source = native_source(&native_manifest("execution", "native"));

    let assert = mur(home.path(), source.path(), None)
        .arg("build")
        .assert()
        .failure();

    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-BLD-007]"), "{stderr}");
    assert!(
        stderr.contains("'nat-tool'") && stderr.contains("execution: native"),
        "{stderr}"
    );
    assert!(built_zips(source.path()).is_empty());
}

#[test]
fn s1_build_refuses_any_other_implementation_as_an_invalid_type() {
    let home = tempfile::tempdir().unwrap();
    let source = native_source(&native_manifest("implementation", "rust"));

    let assert = mur(home.path(), source.path(), None)
        .arg("build")
        .assert()
        .failure();

    let stderr = stderr_of(&assert);
    assert!(stderr.contains("error[E-MAN-003]"), "{stderr}");
    assert!(
        stderr.contains("field 'implementation' has invalid type (expected wasm, got rust)"),
        "{stderr}"
    );
    assert!(!stderr.contains("E-BLD-007"), "{stderr}");
    assert!(built_zips(source.path()).is_empty());
}

#[test]
fn s1_build_accepts_implementation_wasm() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    fs::write(
        source.path().join("murmur.yaml"),
        "name: wasm-tool\nversion: 0.1.0\nruntime: tool\nimplementation: wasm\nrequires_files:\n  \
         - tool.wasm\n",
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/echo-tool.wasm"),
        source.path().join("tool.wasm"),
    )
    .unwrap();

    mur(home.path(), source.path(), None)
        .arg("build")
        .assert()
        .success();

    assert_eq!(built_zips(source.path()), ["wasm-tool-0.1.0.mur.zip"]);
}

// ── S2: mur publish and mur install ─────────────────────────────────────────

/// Asserts `stderr` carries the `E-REG-012` refusal of `<scope>/nat-tool@0.1.0`.
fn assert_install_refusal(stderr: &str, scope: &str) {
    assert!(stderr.contains("error[E-REG-012]"), "{stderr}");
    assert!(
        stderr.contains(&format!("{scope}/{NAT}@{NAT_VERSION}"))
            && stderr.contains("implementation: native"),
        "{stderr}"
    );
}

/// The native zip written to `dir` under the name `mur build` would have given it.
fn native_file(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{NAT}-{NAT_VERSION}.mur.zip"));
    fs::write(&path, native_zip()).unwrap();
    path
}

#[test]
fn s2_publish_refuses_a_native_zip_and_stores_nothing() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let file = native_file(staging.path());

    let assert = mur(home.path(), staging.path(), None)
        .args(["publish", file.to_str().unwrap()])
        .assert()
        .failure();

    let stderr = stderr_of(&assert);
    println!("S2 publish:\n{stderr}");
    assert_install_refusal(&stderr, "local");
    assert!(files_under(&global_store(home.path())).is_empty());
}

#[test]
fn s2_install_of_a_file_refuses_it_into_either_store_and_writes_no_lock() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: p\nversion: 0.0.1\nartifacts: []\n",
    )
    .unwrap();
    let file = native_file(staging.path());

    let project_install = mur(home.path(), project.path(), None)
        .args(["install", "--no-precompile", file.to_str().unwrap()])
        .assert()
        .failure();
    assert_install_refusal(&stderr_of(&project_install), "local");
    assert!(files_under(&project.path().join(".murmur")).is_empty());
    assert!(!project.path().join("murmur.lock").exists());

    let global_install = mur(home.path(), project.path(), None)
        .args(["install", "--no-precompile", "-g", file.to_str().unwrap()])
        .assert()
        .failure();
    assert_install_refusal(&stderr_of(&global_install), "local");
    assert!(files_under(&global_store(home.path())).is_empty());
}

/// `ana/artifacts` on `stub`, release `v1`: `wasm-tool-0.1.0.mur.zip`, the native
/// `nat-tool-0.1.0.mur.zip` and an index listing both. `mur index` refuses the native zip, so the
/// index is written for a WASM stand-in of the same name and its digest swapped for the native
/// payload's.
fn ana_release_with_a_native_tool(stub: &MockGitHub) -> TempDir {
    let staging = tempfile::tempdir().unwrap();
    wasm_tool_zip(staging.path(), "wasm-tool", "0.1.0");
    let stand_in = wasm_tool_zip(staging.path(), NAT, NAT_VERSION);
    let stand_in_sha256 = sha256_hex(&fs::read(&stand_in).unwrap());
    let index = String::from_utf8(mur_index(staging.path(), ANA, "v1", None)).unwrap();
    let native = native_zip();
    let index = index.replace(&stand_in_sha256, &sha256_hex(&native));
    fs::write(&stand_in, &native).unwrap();

    stub.repository(ANA, ANA_OWNER_ID);
    let mut assets = release_assets(staging.path());
    assets.push(("artifacts-index.json".to_string(), index.into_bytes()));
    stub.release(ANA, "v1", assets);
    staging
}

/// The lock at `lock_path` pins `ana/wasm-tool` under `sha256.any` and holds no `ana/nat-tool`.
fn assert_only_the_wasm_tool_is_locked(lock_path: &Path) {
    let lock = read_lockfile(lock_path).unwrap();
    let names: Vec<&str> = lock.artifacts.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["ana/wasm-tool"]);
    assert_eq!(lock.artifacts[0].sha256.any.len(), 64);
    let text = fs::read_to_string(lock_path).unwrap();
    assert!(
        text.contains("any:") && !text.contains("platforms"),
        "{text}"
    );
}

#[test]
fn s2_a_github_release_installs_its_wasm_tool_and_reports_the_native_one_failed() {
    let stub = MockGitHub::start();
    let _staging = ana_release_with_a_native_tool(&stub);
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: p\nversion: 0.0.1\nartifacts: []\n",
    )
    .unwrap();

    let assert = mur(home.path(), project.path(), Some(&stub))
        .args(["install", "--no-precompile", "github:ana/artifacts@v1"])
        .assert()
        .failure();

    let stdout = stdout_of(&assert);
    let stderr = stderr_of(&assert);
    println!("S2 github: stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(
        stdout.contains("Installed ana/wasm-tool@0.1.0 from github:ana/artifacts"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1 of 2 assets of github:ana/artifacts@v1 failed to install:"),
        "{stdout}"
    );
    assert_install_refusal(&stdout, "ana");
    assert!(stderr.contains("error[E-REG-012]"), "{stderr}");
    let project_store = project.path().join(".murmur").join("artifacts");
    assert!(project_store.join("ana/wasm-tool").is_dir());
    assert!(!project_store.join("ana").join(NAT).exists());
    assert_only_the_wasm_tool_is_locked(&project.path().join("murmur.lock"));
}

#[test]
fn s2_a_manifest_install_installs_its_wasm_tool_and_reports_the_native_one_failed() {
    let stub = MockGitHub::start();
    let _staging = ana_release_with_a_native_tool(&stub);
    let home = tempfile::tempdir().unwrap();
    let murmur_dir = home.path().join(".murmur");
    fs::create_dir_all(&murmur_dir).unwrap();
    fs::write(murmur_dir.join("config.yaml"), ANA_CONFIG).unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: p\nversion: 0.0.1\nartifacts:\n  - name: ana/wasm-tool\n    version: 0.1.0\n    \
             runtime: tool\n  - name: ana/{NAT}\n    version: {NAT_VERSION}\n    runtime: tool\n"
        ),
    )
    .unwrap();

    let assert = mur(home.path(), project.path(), Some(&stub))
        .args(["install", "--no-precompile"])
        .assert()
        .failure();

    let stdout = stdout_of(&assert);
    println!("S2 manifest install:\n{stdout}");
    assert!(
        stdout.contains("1 of 2 artifacts failed to install:"),
        "{stdout}"
    );
    assert_install_refusal(&stdout, "ana");
    let project_store = project.path().join(".murmur").join("artifacts");
    assert!(project_store.join("ana/wasm-tool").is_dir());
    assert!(!project_store.join("ana").join(NAT).exists());
    assert_only_the_wasm_tool_is_locked(&project.path().join("murmur.lock"));
}

// ── S3: a launch against a store an older mur left ──────────────────────────

fn s3_launch_refuses(layout: LegacyLayout) {
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let launch = Launch::new();
    let nat_sha256 = seed_legacy_native(&global_store(launch.home.path()), layout);
    launch.write_manifest(&capsule_yaml(&server.endpoint, &nat_entry(), ""));
    launch.write_lock(&[(&format!("local/{NAT}"), NAT_VERSION, &nat_sha256)]);

    let run = launch.run().failure();
    let stderr = stderr_of(&run);
    println!("S3 {layout:?} run:\n{stderr}");
    assert!(stderr.contains("error[E-RUN-061]"), "{stderr}");
    assert!(
        stderr.contains(&format!("local/{NAT}")) && stderr.contains("implementation"),
        "{stderr}"
    );
    assert!(!stderr.contains("unknown variant"), "{stderr}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());

    let doctor = launch.mur().arg("doctor").assert().failure();
    let stdout = stdout_of(&doctor);
    println!("S3 {layout:?} doctor:\n{stdout}");
    assert!(
        stdout.contains(&format!("local/{NAT}@{NAT_VERSION}")) && stdout.contains("E-RUN-061"),
        "{stdout}"
    );
    assert!(!stdout.contains("unknown variant"), "{stdout}");

    let list = launch.mur().args(["list", "-g"]).assert().success();
    let stdout = stdout_of(&list);
    assert!(stdout.contains(&format!("local/{NAT}")), "{stdout}");
}

#[test]
fn s3_a_launch_refuses_a_tagged_native_install_an_older_mur_left() {
    s3_launch_refuses(LegacyLayout::Tagged);
}

#[test]
fn s3_a_launch_refuses_a_generic_native_install_an_older_mur_left() {
    s3_launch_refuses(LegacyLayout::Generic);
}

// ── S4: a runtime pull ───────────────────────────────────────────────────────

#[test]
fn s4_a_running_capsule_cannot_pull_a_native_tool() {
    if capsule_runtime::skip_without_egress_namespace(
        "s4_a_running_capsule_cannot_pull_a_native_tool",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_use(
            "toolu_pull",
            "pull-artifact",
            json!({"artifact": format!("local/{NAT}")}),
        ),
        end_turn("done"),
    ]);
    let launch = Launch::new();
    seed_legacy_native(&global_store(launch.home.path()), LegacyLayout::Generic);
    launch.write_manifest(&capsule_yaml(
        &server.endpoint,
        "",
        "  install:\n    tool: [\"local/*\"]\n",
    ));

    let run = launch.run().success();
    let stdout = stdout_of(&run);
    assert!(stdout.contains("status:  ok"), "{stdout}");
    let workdir = common::parse_workdir_from_stdout(&stdout);

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    let block = common::find_tool_result(&requests, "toolu_pull").expect("a pull result");
    let text = common::extract_result_text(&block);
    println!("S4 pull result: {text}");
    assert!(
        text.contains("E-RUN-061") && text.contains(NAT) && text.contains("implementation"),
        "{text}"
    );
    assert!(files_under(&workdir.join("tools").join(NAT)).is_empty());
    if let Ok(lock) = read_lockfile(&launch.lock_path()) {
        assert!(
            lock.artifact_for(&format!("local/{NAT}")).is_none(),
            "{lock:?}"
        );
    }
}

// ── S5: removed surface ──────────────────────────────────────────────────────

#[test]
fn s5_the_per_platform_flags_are_unknown_arguments() {
    let home = tempfile::tempdir().unwrap();
    for (args, flag) in [
        (
            vec!["install", "--all-platforms", "local/x@1"],
            "--all-platforms",
        ),
        (
            vec!["publish", "--platform", "linux-x86_64", "a.mur.zip"],
            "--platform",
        ),
    ] {
        let assert = mur(home.path(), home.path(), None)
            .args(&args)
            .assert()
            .code(2);
        let stderr = stderr_of(&assert);
        assert!(
            stderr.contains(&format!("unexpected argument '{flag}'")),
            "{stderr}"
        );
    }
}

#[test]
fn s5_list_prints_no_platforms_column() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let tool = wasm_tool_zip(staging.path(), "wasm-tool", "0.1.0");
    common::publish_local(&home, &tool).success();

    let assert = mur(home.path(), home.path(), None)
        .args(["list", "-g"])
        .assert()
        .success();

    let stdout = stdout_of(&assert);
    let header = stdout.lines().next().unwrap_or_default();
    assert_eq!(
        header.split_whitespace().collect::<Vec<_>>(),
        ["NAME", "VERSION", "RUNTIME"]
    );
    assert!(!stdout.contains("PLATFORMS"), "{stdout}");
    assert!(stdout.contains("local/wasm-tool"), "{stdout}");
}

// ── S6: lock and index ───────────────────────────────────────────────────────

#[test]
fn s6_a_lock_entry_pinning_per_platform_hashes_is_refused() {
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let launch = Launch::new();
    launch.write_manifest(&capsule_yaml(&server.endpoint, "", ""));
    let platform = host_platform();
    fs::write(
        launch.lock_path(),
        format!(
            "lock_version: 3\nartifacts:\n- name: {DRIVER_NAME}\n  resolved_version: \
             {DRIVER_VERSION}\n  sha256:\n    platforms:\n      {platform}: {}\n  origin: \
             operator\n",
            launch.driver_sha256
        ),
    )
    .unwrap();

    let run = launch.run().failure();
    let stderr = stderr_of(&run);
    println!("S6 platforms lock:\n{stderr}");
    assert!(stderr.contains("error[E-RUN-003]"), "{stderr}");
    assert!(
        stderr.contains(DRIVER_NAME) && stderr.contains("sha256.platforms"),
        "{stderr}"
    );
    assert!(stderr.contains("mur install"), "{stderr}");
    assert!(server.requests().is_empty());
}

#[test]
fn s6_a_lock_entry_pinning_any_launches() {
    if capsule_runtime::skip_without_egress_namespace("s6_a_lock_entry_pinning_any_launches") {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("ok")]);
    let launch = Launch::new();
    launch.write_manifest(&capsule_yaml(&server.endpoint, "", ""));
    launch.write_lock(&[]);

    let run = launch.run().success();
    let stdout = stdout_of(&run);
    assert!(stdout.contains("status:  ok"), "{stdout}");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn s6_index_refuses_a_platform_tagged_file_name() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let built = wasm_tool_zip(dir.path(), "echo-tool", "0.1.0");
    let tagged = dir.path().join("echo-tool-0.1.0-linux-x86_64.mur.zip");
    fs::rename(&built, &tagged).unwrap();
    let out = home.path().join("artifacts-index.json");

    let assert = mur(home.path(), home.path(), None)
        .args([
            "index",
            dir.path().to_str().unwrap(),
            "--repo",
            ANA,
            "--release",
            "v1",
            "-o",
            out.to_str().unwrap(),
        ])
        .assert()
        .failure();

    let stderr = stderr_of(&assert);
    println!("S6 index:\n{stderr}");
    assert!(stderr.contains("error[E-IDX-001]"), "{stderr}");
    assert!(
        stderr.contains("echo-tool-0.1.0-linux-x86_64.mur.zip"),
        "{stderr}"
    );
    assert!(!out.exists());
}

#[test]
fn s6_an_index_of_correctly_named_files_carries_no_platform() {
    let dir = tempfile::tempdir().unwrap();
    wasm_tool_zip(dir.path(), "echo-tool", "0.1.0");
    wasm_tool_zip(dir.path(), "other-tool", "0.2.0");

    let index: Value = serde_json::from_slice(&mur_index(dir.path(), ANA, "v1", None)).unwrap();

    let assets: Vec<&Value> = index["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|artifact| artifact["versions"].as_array().unwrap())
        .flat_map(|version| version["assets"].as_array().unwrap())
        .collect();
    assert_eq!(assets.len(), 2, "{index:#}");
    for asset in assets {
        assert!(asset.get("platform").is_none(), "{asset}");
    }
}
