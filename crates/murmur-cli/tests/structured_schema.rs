#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn mur_run_input_flag_writes_input_json() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let output = run_mur(
        &home,
        &manifest_path,
        &["--task", "Build X", "--instructions", "Be precise"],
    );
    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());

    let input_json: Value = read_json(&workdir.join("input.json"));
    assert_eq!(input_json["schema"], "murmur.message.v1");
    assert_eq!(input_json["type"], "murmur.code_task.request.v1");
    assert_eq!(input_json["payload"]["objective"], "Build X");
    assert_eq!(input_json["payload"]["instructions"], "Be precise");

    let requests = server.requests();
    let task_text = requests[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(task_text.starts_with("Objective: Build X"));
    assert!(task_text.contains("Instructions: Be precise"));
}

#[test]
fn mur_run_input_flag_reads_from_file() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let task_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(task_file.path(), "Build from file").unwrap();

    let arg = format!("@{}", task_file.path().display());
    let output = run_mur(&home, &manifest_path, &["--task", &arg]);
    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());

    let input_json: Value = read_json(&workdir.join("input.json"));
    assert_eq!(input_json["payload"]["objective"], "Build from file");
}

#[test]
fn mur_run_no_flags_falls_back_to_task_md() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let project_dir = manifest_path.parent().unwrap();
    fs::write(project_dir.join("task.md"), "Legacy markdown task").unwrap();

    let output = run_mur(&home, &manifest_path, &[]);
    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());

    assert!(!project_dir.join("input.json").exists());
    assert!(!workdir.join("input.json").exists());

    let requests = server.requests();
    let task_text = requests[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(task_text, "Legacy markdown task");
}

#[test]
fn result_json_written_on_clean_completion() {
    let server = end_turn_server("structured output");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let output = run_mur(&home, &manifest_path, &["--task", "Finish cleanly"]);
    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());

    let result_txt = fs::read_to_string(workdir.join("out/result.txt")).unwrap();
    let result_json: Value = read_json(&workdir.join("out/result.json"));
    assert_eq!(result_txt, "structured output");
    assert_eq!(result_json["schema"], "murmur.message.v1");
    assert_eq!(result_json["type"], "murmur.code_task.result.v1");
    assert!(result_json["payload"]["status"].is_null());
    assert_eq!(result_json["payload"]["output"], result_txt);
}

#[test]
fn result_json_not_written_on_error() {
    let server = common::ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [],
        "stop_reason": "error",
        "error": "driver-side failure",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged =
        common::stage_agent_session(&home, manifest_path.parent().unwrap(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Trigger an error").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent error response should not trap");
    assert!(launched.workdir.join("out/result.txt").exists());
    assert!(!launched.workdir.join("out/result.json").exists());
}

#[test]
fn job_id_written_to_meta_dir_for_agent_result_json() {
    let server = end_turn_server("worker output");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent_session_with_job_id(
        &home,
        manifest_path.parent().unwrap(),
        &manifest_path,
        Some("job-123".to_string()),
    );
    fs::create_dir_all(staged.workdir.join("meta")).unwrap();
    fs::write(staged.workdir.join("meta/job_id.txt"), "job-123").unwrap();
    fs::write(staged.workdir.join("task.md"), "Run as worker").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    assert_eq!(
        fs::read_to_string(launched.workdir.join("meta/job_id.txt")).unwrap(),
        "job-123"
    );
    let result_json: Value = read_json(&launched.workdir.join("out/result.json"));
    assert_eq!(result_json["job_id"], "job-123");
}

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: structured-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()])
}

fn run_mur(home: &TempDir, manifest_path: &Path, extra_args: &[&str]) -> Vec<u8> {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    let mut args = vec!["run", "--manifest", manifest_path.to_str().unwrap()];
    args.extend_from_slice(extra_args);

    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(args)
        .assert()
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone()
}

fn stage_agent_session_with_job_id(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
    job_id: Option<String>,
) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();

    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project_dir.to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
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
            job_id,
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .unwrap()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn parse_workdir_from_stdout(stdout: &str) -> PathBuf {
    let marker = "workdir: ";
    let after = stdout
        .split(marker)
        .nth(1)
        .unwrap_or_else(|| panic!("missing workdir marker in stdout: {stdout}"));
    PathBuf::from(after.lines().next().unwrap_or_default().trim())
}
