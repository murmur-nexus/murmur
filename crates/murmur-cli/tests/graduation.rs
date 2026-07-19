#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    LockExpectation, StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, read_lockfile, write_lockfile_atomic, ArtifactRuntime, LocalRegistry,
    LockedArtifact, LockedSha256, LockfileError, MurmurLock, LOCK_VERSION,
};
use predicates::prelude::*;
use tempfile::TempDir;

const TOOL_NAME: &str = "jsonl-line-count";
const TOOL_VERSION: &str = "0.1.0";

#[test]
fn graduation_full_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    copy_dir_all(&fixture_path("graduation/capsule"), project.path());

    let artifact_path = artifact_dir.path().join("jsonl-line-count-0.1.0.mur.zip");
    build_tool_fixture(&fixture_path("graduation/tool"), &artifact_path)
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    common::publish_local(&home, &artifact_path)
        .success()
        .stdout(predicate::str::contains("Published jsonl-line-count@0.1.0"));

    let launched_workdir = stage_and_launch(&home, project.path());

    assert_eq!(
        fs::read_to_string(launched_workdir.join("out/result.txt")).unwrap(),
        "5"
    );

    let lock_path = project.path().join("murmur.lock");
    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock
        .artifact_for(TOOL_NAME)
        .expect("lock entry for jsonl-line-count");
    assert_eq!(entry.resolved_version, TOOL_VERSION);
    assert!(!entry.sha256.wasm.is_empty());
}

#[test]
fn graduation_second_run_uses_lock() {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    copy_dir_all(&fixture_path("graduation/capsule"), project.path());

    let artifact_path = artifact_dir.path().join("jsonl-line-count-0.1.0.mur.zip");
    build_tool_fixture(&fixture_path("graduation/tool"), &artifact_path)
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    common::publish_local(&home, &artifact_path)
        .success()
        .stdout(predicate::str::contains("Published jsonl-line-count@0.1.0"));

    let first_workdir = stage_and_launch(&home, project.path());
    assert_eq!(
        fs::read_to_string(first_workdir.join("out/result.txt")).unwrap(),
        "5"
    );

    let lock_path = project.path().join("murmur.lock");
    let lock_before = fs::read(&lock_path).unwrap();

    let second_workdir = stage_and_launch(&home, project.path());
    assert_eq!(
        fs::read_to_string(second_workdir.join("out/result.txt")).unwrap(),
        "5"
    );

    let lock_after = fs::read(&lock_path).unwrap();
    assert_eq!(lock_before, lock_after);
}

#[test]
fn graduation_bad_artifact_version_surfaces_error() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::copy(
        fixture_path("graduation/capsule/capsule.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        "name: graduation-capsule\nversion: 0.1.0\nartifacts:\n  - name: jsonl-line-count\n    version: 9.9.9\n    runtime: tool\n",
    )
    .unwrap();

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .failure()
        .stderr(predicate::str::contains("error[E-RUN-008]:"))
        .stderr(predicate::str::contains("missing artifacts: jsonl-line-count@9.9.9"))
        .stderr(predicate::str::contains("run `mur install`"));

    assert!(!project.path().join("workdir").exists());
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(relative)
}

fn build_tool_fixture(source: &Path, output: &Path) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.args([
        "build",
        source.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
    ])
    .assert()
}

fn stage_and_launch(home: &TempDir, project_dir: &Path) -> PathBuf {
    let manifest_path = project_dir.join("murmur.yaml");
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::with_capacity(runtime_manifest.artifacts.len());
    for artifact in &runtime_manifest.artifacts {
        assert!(
            artifact.runtime == ArtifactRuntime::Tool,
            "graduation fixture only supports wasm runtime artifacts"
        );

        allowlisted_tools.insert(artifact.name.clone());
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
        });
    }

    let lock_path = project_dir.join("murmur.lock");
    let (staged_artifacts, lock_expectations, write_lock_after_stage) =
        match read_lockfile(&lock_path) {
            Ok(lock) => {
                let mut pinned_artifacts = Vec::with_capacity(requested_artifacts.len());
                let mut expectations = Vec::with_capacity(requested_artifacts.len());

                for artifact in &requested_artifacts {
                    let entry = lock
                        .artifact_for(&artifact.name)
                        .expect("lock entry for requested artifact");
                    pinned_artifacts.push(ArtifactRequest {
                        name: artifact.name.clone(),
                        version: entry.resolved_version.clone(),
                        runtime: artifact.runtime.clone(),
                        source: artifact.source.clone(),
                    });
                    expectations.push(LockExpectation {
                        name: artifact.name.clone(),
                        resolved_version: entry.resolved_version.clone(),
                        sha256_wasm: entry.sha256.wasm.clone(),
                    });
                }

                (pinned_artifacts, Some(expectations), false)
            }
            Err(LockfileError::NotFound(_)) => (requested_artifacts, None, true),
            Err(error) => panic!("failed to read lockfile: {error}"),
        };

    let capsule_component_bytes = fs::read(project_dir.join("capsule.wasm")).unwrap();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project_dir.to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes,
            artifacts: staged_artifacts,
            allowlisted_tools,
            lock_expectations,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            context: runtime_manifest.context.clone(),
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            job_id: None,
        },
    )
    .unwrap();

    if write_lock_after_stage {
        let lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: staged
                .resolved_lock_artifacts
                .iter()
                .map(|entry| LockedArtifact {
                    name: entry.name.clone(),
                    resolved_version: entry.resolved_version.clone(),
                    sha256: LockedSha256 {
                        wasm: entry.sha256_wasm.clone(),
                    },
                })
                .collect(),
        };
        write_lockfile_atomic(&lock_path, &lock).unwrap();
    }

    launch_session(staged, |_| {}).unwrap().workdir
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
