//! A `transport: process` capsule names its process driver under `inference.driver`. The runtime
//! loads the driver with no grants, checks its exports against the transport, asks it to describe
//! itself and checks its required environment, then refuses the launch because nothing runs a
//! process driver yet.
//!
//! Driven through the real `mur` binary against a temp `HOME` local store and a temp project.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::process_driver::{describe_process_driver_wasm, InterruptMethod};
use tempfile::TempDir;

const DRIVER: &str = "fixture-process-driver";
const VERSION: &str = "0.1.0";

fn fixture_wasm() -> PathBuf {
    common::fixture_path("process-driver/tool/process-driver.wasm")
}

fn anthropic_wasm() -> PathBuf {
    common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm")
}

/// A published driver artifact and a project holding the manifest that names it.
struct Capsule {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

impl Capsule {
    /// Publishes `wasm` as `runtime: driver` artifact `name`, with no `inference_auth:`, and
    /// writes a manifest declaring it with `entry_extra` appended to its entry, `top_extra` at the
    /// top level, and `inference` as the whole `inference:` block.
    fn new(name: &str, wasm: &Path, entry_extra: &str, top_extra: &str, inference: &str) -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let artifact =
            common::create_driver_artifact_with_auth(artifacts.path(), name, VERSION, wasm, "");
        common::publish_local(&home, &artifact).success();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: process-capsule\nversion: 0.1.0\nartifacts:\n  - name: {name}\n    \
                 version: {VERSION}\n    runtime: driver\n{entry_extra}{top_extra}{inference}"
            ),
        )
        .unwrap();
        Self {
            home,
            project,
            manifest,
        }
    }

    /// A process capsule naming `name` as its driver, with no `command:`.
    fn process(name: &str, wasm: &Path, env_allow: &[&str]) -> Self {
        Self::new(
            name,
            wasm,
            "",
            &format!(
                "capabilities:\n  env:\n    allow: [{}]\n",
                env_allow.join(", ")
            ),
            &format!("inference:\n  transport: process\n  driver:\n    artifact: {name}\n"),
        )
    }

    /// Combined stdout and stderr of a failed `mur run`.
    fn run_refused(&self) -> String {
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                "hi",
                "--verbose",
            ])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "mur run succeeded:\n{text}");
        text
    }
}

/// Every `*.mur.zip` under `dir`, recursively.
fn find_zips(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            find_zips(&path, found);
        } else if path.to_string_lossy().ends_with(".mur.zip") {
            found.push(path);
        }
    }
}

#[test]
fn fixture_describes_itself() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        VERSION,
        &fixture_wasm(),
        "",
    );
    common::publish_local(&home, &artifact).success();

    let mut published = Vec::new();
    find_zips(home.path(), &mut published);
    let published = published
        .into_iter()
        .find(|path| path.to_string_lossy().contains(DRIVER))
        .expect("the published driver is in the local store");
    let mut archive = zip::ZipArchive::new(fs::File::open(published).unwrap()).unwrap();
    let mut wasm = Vec::new();
    archive
        .by_name("tool.wasm")
        .unwrap()
        .read_to_end(&mut wasm)
        .unwrap();

    let description = describe_process_driver_wasm(&wasm).unwrap();
    assert_eq!(description.harness, "fixture-harness");
    assert_eq!(description.binary, "fixture-cli");
    assert_eq!(description.version_args, vec!["--version".to_string()]);
    assert_eq!(description.tested_versions, vec!["1.0.0".to_string()]);
    assert!(matches!(
        description.interrupt,
        InterruptMethod::StdinMessage
    ));
    assert_eq!(
        description.required_env,
        vec!["HOME".to_string(), "FIXTURE_HARNESS_PROFILE".to_string()]
    );
    assert!(description.streams_text);
}

#[test]
fn process_driver_is_refused_as_not_wired() {
    let capsule = Capsule::process(
        DRIVER,
        &fixture_wasm(),
        &["HOME", "FIXTURE_HARNESS_PROFILE"],
    );
    let text = capsule.run_refused();
    assert!(text.contains("E-RUN-031"), "{text}");
    assert!(text.contains("fixture-process-driver@0.1.0"), "{text}");
    assert!(text.contains("fixture-harness"), "{text}");
    assert!(text.contains("fixture-cli"), "{text}");
    assert!(!text.contains("E-RUN-025"), "{text}");
    assert!(!text.contains("E-RUN-006"), "{text}");
}

#[test]
fn missing_required_env_is_refused() {
    let capsule = Capsule::process(DRIVER, &fixture_wasm(), &["HOME"]);
    let text = capsule.run_refused();
    assert!(text.contains("E-CAP-019"), "{text}");
    assert!(text.contains("FIXTURE_HARNESS_PROFILE"), "{text}");
    assert!(text.contains("fixture-process-driver"), "{text}");
    assert!(!text.contains("E-RUN-031"), "{text}");
}

#[test]
fn http_driver_under_process_is_refused() {
    let name = "murmur-driver-anthropic";
    let capsule = Capsule::process(name, &anthropic_wasm(), &["HOME"]);
    let text = capsule.run_refused();
    assert!(text.contains("E-RUN-029"), "{text}");
    assert!(text.contains("murmur-driver-anthropic@0.1.0"), "{text}");
    assert!(text.contains("murmur:driver/process@0.1.0"), "{text}");
}

#[test]
fn process_driver_under_http_is_refused() {
    let capsule = Capsule::new(
        DRIVER,
        &fixture_wasm(),
        "    gateway:\n      endpoint: \"http://127.0.0.1:9\"\n      keyless: true\n",
        "",
        &format!(
            "inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n"
        ),
    );
    let text = capsule.run_refused();
    assert!(text.contains("E-RUN-030"), "{text}");
    assert!(text.contains("fixture-process-driver"), "{text}");
    assert!(!text.contains("E-RUN-025"), "{text}");
}

/// `mur install` never reads `inference_auth:`, so a process driver, which declares none,
/// installs like any other artifact.
#[test]
fn install_accepts_process_driver_without_inference_auth() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        VERSION,
        &fixture_wasm(),
        "",
    );
    fs::write(
        project.path().join("murmur.yaml"),
        "name: install-capsule\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.path())
        .args(["install", artifact.to_str().unwrap()])
        .assert()
        .success();
}
