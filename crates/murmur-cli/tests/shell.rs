#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn shell_execute_bash_and_reads_stdout() {
    if common::skip_without_host_support("shell_execute_bash_and_reads_stdout") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_ls",
                "name": "bash",
                "input": {"command": "ls"}
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
            "content": [{"type": "text", "text": "Directory listing received."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run ls and summarize it.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("Directory listing received."));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_ls").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.to_lowercase().contains("exit code"));
}

#[test]
fn shell_blocks_undeclared_binary() {
    if common::skip_without_host_support("shell_blocks_undeclared_binary") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_curl",
                "name": "curl",
                "input": {"command": "curl --version"}
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
            "content": [{
                "type": "text",
                "text": "tool 'curl' is not declared in manifest allowlist"
            }],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Try curl").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should not trap");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("curl"));
    assert!(output.contains("allowlist"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_curl").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.contains("curl"));
    assert!(text.contains("not declared in manifest allowlist"));
}

#[test]
fn shell_propagates_nonzero_exit_code() {
    if common::skip_without_host_support("shell_propagates_nonzero_exit_code") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_exit",
                "name": "bash",
                "input": {"command": "exit 42"}
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
            "content": [{"type": "text", "text": "Observed non-zero exit."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.workdir.join("task.md"),
        "Run a failing bash command.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    assert!(launched.workdir.join("out/result.txt").exists());

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_exit").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.contains("42"));
}

fn find_tool_result_block<'a>(request: &'a Value, tool_use_id: &str) -> Option<&'a Value> {
    let messages = request.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };

        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
            {
                return Some(block);
            }
        }
    }

    None
}

fn tool_result_text(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_string();
    }

    block
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
        })
        .unwrap_or_default()
}

fn stage_agent_session(home: &TempDir, project_dir: &Path, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn create_agent_project(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    shell_allow: &[&str],
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let shell_allow_yaml = shell_allow
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: agent-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver_name}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n{shell_allow_yaml}inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {driver_name}\n"
        ),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}
