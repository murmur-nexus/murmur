#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, RuntimeError, StagedSession};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn inline_system_prompt_appears_in_every_api_call() {
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let system_prompt = "Always begin every response with CONFIRMED:";
    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        &format!("  system_prompt: \"{system_prompt}\"\n"),
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected two inference requests");
    for request in requests {
        let system_field = request["system"].as_str().expect("system field should be present");
        assert!(
            system_field.starts_with("[Capsule]\nName:"),
            "system field should start with [Capsule] block; got:\n{system_field}"
        );
        assert!(
            system_field.contains(system_prompt),
            "system field should contain the manifest system prompt; got:\n{system_field}"
        );
    }
}

#[test]
fn system_prompt_file_contents_injected_as_system_param() {
    let server = ScriptedServer::start(vec![one_turn_response()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let conventions =
        "Follow architecture rules strictly.\nAlways prefix final answers with CONFIRMED:.";
    fs::write(project.path().join("conventions.md"), conventions).unwrap();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        "  system_prompt_file: conventions.md\n",
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Say hello.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected one inference request");
    let system_field = requests[0]["system"].as_str().expect("system field should be present");
    assert!(
        system_field.starts_with("[Capsule]\nName:"),
        "system field should start with [Capsule] block; got:\n{system_field}"
    );
    assert!(
        system_field.contains(conventions),
        "system field should contain the system_prompt_file contents; got:\n{system_field}"
    );
}

#[test]
fn missing_system_prompt_file_fails_at_launch() {
    let server = ScriptedServer::start(Vec::new());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        "  system_prompt_file: missing-conventions.md\n",
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Say hello.").unwrap();

    let err =
        launch_session(staged, |_| {}).expect_err("launch should fail for missing prompt file");

    match err {
        RuntimeError::SystemPromptFileRead { path, .. } => {
            assert!(path.ends_with("missing-conventions.md"));
        }
        other => panic!("expected SystemPromptFileRead, got: {other:?}"),
    }

    assert_eq!(
        server.requests().len(),
        0,
        "launch failure should occur before any inference call"
    );
}

#[test]
fn no_system_prompt_field_sends_no_system_param() {
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(project.path(), &server.endpoint, "");

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected two inference requests");
    for request in requests {
        let system_field = request["system"].as_str().expect("system field should be present");
        assert!(
            system_field.starts_with("[Capsule]\nName:"),
            "system field should contain [Capsule] identity block even with no manifest system prompt; got:\n{system_field}"
        );
    }
}

fn stage_agent_session(home: &TempDir, project_dir: &Path, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

fn create_agent_manifest(project_dir: &Path, endpoint: &str, inference_extra: &str) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let manifest = format!(
        concat!(
            "name: system-prompt-test\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "{inference_extra}",
        ),
        driver_name = DRIVER_ANTHROPIC_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        inference_extra = inference_extra,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

fn one_turn_response() -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn two_turn_responses() -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_bash",
                "name": "bash",
                "input": {"command": "echo hello"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]
}
