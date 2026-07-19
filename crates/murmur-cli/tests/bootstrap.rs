#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "jsonl-line-count";
const TOOL_VERSION: &str = "0.1.0";
const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
const DRIVER_OPENAI_NAME: &str = "murmur-driver-openai";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn murmur_driver_anthropic_completes_one_tool_task() {
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": TOOL_NAME,
                "input": {"data": "input.jsonl"}
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
            "content": [{"type": "text", "text": "The file contains 5 lines."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        artifact_dir.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );
    common::publish_local(&home, &artifact_path).success();
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
        true,
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.workdir.join("task.md"),
        "Count lines in input.jsonl and write the result.",
    )
    .unwrap();
    fs::write(
        staged.workdir.join("input.jsonl"),
        "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n{\"id\":4}\n{\"id\":5}\n",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("5"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected exactly two inference requests");

    assert_eq!(requests[0]["tools"][0]["name"], TOOL_NAME);
    assert!(requests[0]["tools"][0].get("input_schema").is_some());

    let second_messages = requests[1]["messages"].as_array().unwrap();
    let has_tool_result = second_messages.iter().any(|message| {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            return false;
        }

        message
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("tool_use_id").and_then(Value::as_str) == Some("toolu_1")
                })
            })
            .unwrap_or(false)
    });

    assert!(
        has_tool_result,
        "second request should include tool_result for toolu_1"
    );
}

#[test]
fn murmur_driver_openai_completes_one_tool_task() {
    let server = ScriptedServer::start(vec![
        json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [{
                        "id": "tc-1",
                        "type": "function",
                        "function": {
                            "name": TOOL_NAME,
                            "arguments": "{\"data\":\"input.jsonl\"}"
                        }
                    }]
                }
            }]
        })
        .to_string(),
        json!({
            "id": "chatcmpl-2",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "The file contains 5 lines."
                }
            }]
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        artifact_dir.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );
    common::publish_local(&home, &artifact_path).success();
    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_OPENAI_NAME,
        &fixture_path("drivers/openai/driver/murmur-driver-openai.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path =
        create_agent_project(project.path(), &server.endpoint, DRIVER_OPENAI_NAME, true);

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.workdir.join("task.md"),
        "Count lines in input.jsonl and write the result.",
    )
    .unwrap();
    fs::write(
        staged.workdir.join("input.jsonl"),
        "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n{\"id\":4}\n{\"id\":5}\n",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("5"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected exactly two inference requests");

    let first_messages = requests[0]["messages"].as_array().unwrap();
    assert_eq!(first_messages[0]["role"], "system");
    assert_eq!(first_messages[1]["role"], "user");
    assert_eq!(requests[0]["tools"][0]["type"], "function");

    let second_messages = requests[1]["messages"].as_array().unwrap();
    let has_tool_message = second_messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("tool")
            && message.get("tool_call_id").and_then(Value::as_str) == Some("tc-1")
    });
    assert!(
        has_tool_message,
        "second request should include tool message"
    );
}

#[test]
fn bootstrap_rejects_undeclared_tool() {
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_bad",
                "name": "non-existent-tool",
                "input": {"data": "input.jsonl"}
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
                "text": "tool 'non-existent-tool' is not declared in manifest allowlist"
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

    let artifact_path = create_tool_artifact(
        artifact_dir.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );
    common::publish_local(&home, &artifact_path).success();
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
        true,
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Try a missing tool").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should not trap");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("non-existent-tool"));
    assert!(output.contains("not declared"));

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected agent to send follow-up request"
    );

    let second_messages = requests[1]["messages"].as_array().unwrap();
    let has_error_tool_result = second_messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks.iter().any(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("tool_use_id").and_then(Value::as_str) == Some("toolu_bad")
                        && block["content"][0]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("not declared")
                })
            })
            .unwrap_or(false)
    });

    assert!(
        has_error_tool_result,
        "second request should include tool_result for toolu_bad"
    );
}

/// Without an inference block, `mur run` requires a capsule.wasm — verifies that
/// agent capsules (inference configured) do NOT require a WASM file.
#[test]
fn script_capsule_without_wasm_fails_with_run_004() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Write a manifest with no inference block and no capsule.wasm — should fail.
    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: script-capsule\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .failure()
        .stderr(predicate::str::contains("error[E-RUN-004]"))
        .stderr(predicate::str::contains("no capsule component found"));
}

#[test]
fn bootstrap_missing_driver_artifact_fails_before_launch() {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        artifact_dir.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );

    // Create the project first so find_project_root() works for install.
    let manifest_path = create_agent_project(
        project.path(),
        "http://127.0.0.1:8080",
        DRIVER_ANTHROPIC_NAME,
        true,
    );

    // Install the tool artifact into the project store so that check passes,
    // but do NOT install the driver — that's the artifact we expect to fail on.
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("missing artifacts: murmur-driver-anthropic@"))
        .stderr(predicate::str::contains("run `mur install`"))
        .stderr(predicate::str::contains("error[E-RUN-008]"));
}

#[test]
fn bootstrap_driver_excluded_from_tool_list() {
    let server = ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        artifact_dir.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );
    common::publish_local(&home, &artifact_path).success();
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
        true,
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Do nothing.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert!(
        !requests.is_empty(),
        "expected at least one inference request"
    );

    let tools = requests[0]["tools"].as_array();
    assert!(tools.is_some(), "request should include a tools array");
    let tool_names: Vec<&str> = tools
        .unwrap()
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();

    assert!(
        tool_names.contains(&TOOL_NAME),
        "wasm artifact should appear in tools list"
    );
    assert!(
        !tool_names.iter().any(|&n| n == DRIVER_ANTHROPIC_NAME),
        "driver artifact should not appear in tools list"
    );
}

/// The agent loop prepends a [Capsule] identity block to the system prompt on every inference turn.
/// This test verifies: (a) the block is always first, (b) name and version are correct,
/// (c) the manifest system prompt appears after the block.
#[test]
fn agent_system_prompt_includes_capsule_context() {
    let server = ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_system_prompt = "You are a helpful assistant.";
    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            concat!(
                "name: context-capsule\n",
                "version: 2.3.4\n",
                "artifacts:\n",
                "  - name: {driver}\n",
                "    version: {dver}\n",
                "    runtime: driver\n",
                "capabilities:\n",
                "  network:\n",
                "    allow:\n",
                "      - {endpoint}\n",
                "inference:\n",
                "  transport: http\n",
                "  endpoint: {endpoint}\n",
                "  model: test-model\n",
                "  api_key: test-key\n",
                "  driver:\n",
                "    artifact: {driver}\n",
                "  system_prompt: \"{prompt}\"\n",
            ),
            driver = DRIVER_ANTHROPIC_NAME,
            dver = DRIVER_VERSION,
            endpoint = server.endpoint,
            prompt = manifest_system_prompt,
        ),
    )
    .unwrap();

    let staged =
        stage_agent_session(&home, project.path(), &project.path().join("murmur.yaml"));
    fs::write(staged.workdir.join("task.md"), "hello").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert!(!requests.is_empty(), "expected at least one inference request");

    let system_field = requests[0]["system"]
        .as_str()
        .expect("system field should be present in inference request");

    assert!(
        system_field.starts_with("[Capsule]\nName:"),
        "[Capsule] block must be first in system field; got:\n{system_field}"
    );
    assert!(
        system_field.contains("Name: context-capsule"),
        "system field should contain capsule name; got:\n{system_field}"
    );
    assert!(
        system_field.contains("Version: 2.3.4"),
        "system field should contain capsule version; got:\n{system_field}"
    );
    assert!(
        system_field.contains("Workdir:"),
        "system field should contain Workdir; got:\n{system_field}"
    );
    assert!(
        system_field.contains(manifest_system_prompt),
        "system field should contain the manifest system prompt; got:\n{system_field}"
    );

    let capsule_block_pos = system_field.find("[Capsule]").unwrap();
    let manifest_prompt_pos = system_field.find(manifest_system_prompt).unwrap();
    assert!(
        capsule_block_pos < manifest_prompt_pos,
        "[Capsule] block should appear before the manifest system prompt"
    );
}

fn stage_agent_session(home: &TempDir, project_dir: &Path, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn create_agent_project(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    include_tool_artifact: bool,
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    // No capsule.wasm — agent capsules are manifest-only.

    let mut artifacts_yaml = if include_tool_artifact {
        format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n    runtime: tool\n")
    } else {
        String::new()
    };

    artifacts_yaml.push_str(&format!(
        "  - name: {driver_name}\n    version: {DRIVER_VERSION}\n    runtime: driver\n"
    ));

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: agent-capsule\nversion: 0.1.0\nartifacts:\n{artifacts_yaml}capabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {driver_name}\n"
        ),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_tool_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    tool_component_path: &Path,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();
    writeln!(zip, "description: counts JSONL lines").unwrap();
    writeln!(zip, "input_schema:").unwrap();
    writeln!(zip, "  type: object").unwrap();
    writeln!(zip, "  properties:").unwrap();
    writeln!(zip, "    data:").unwrap();
    writeln!(zip, "      type: string").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(tool_component_path).unwrap())
        .unwrap();

    zip.finish().unwrap();
    artifact_path
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

use common::ScriptedServer;
