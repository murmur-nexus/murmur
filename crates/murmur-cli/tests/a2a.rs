#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![serde_json::json!({
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

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
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
            "name: a2a-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = std::collections::HashSet::new();
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
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
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
            job_id: None,
        },
    )
    .unwrap()
}

/// Send an HTTP POST with a JSON body; return the parsed JSON response body.
fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    // Skip headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    let mut response_body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        response_body.push_str(&line);
    }
    serde_json::from_str(&response_body)
        .unwrap_or_else(|_| serde_json::json!({"_raw": response_body}))
}

#[test]
fn a2a_message_send_starts_agent_loop_and_returns_submitted() {
    let server = end_turn_server("A2A task complete");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    // Stage WITHOUT writing task.md (A2A path)
    let staged = stage_agent(&home, &manifest_path);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    // Send A2A message/send
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "test-msg-1",
                "role": "user",
                "parts": [{"text": "echo hello from A2A"}]
            }
        }
    })
    .to_string();

    let response = http_post_json(&capsule_url, "/", &body);

    // Verify JSON-RPC 2.0 structure and submitted state
    assert_eq!(
        response["jsonrpc"], "2.0",
        "response should be JSON-RPC 2.0; got: {response}"
    );
    assert!(
        response["error"].is_null(),
        "should have no error; got: {response}"
    );
    let task = &response["result"];
    assert!(
        task["id"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "task id should be present; got: {response}"
    );
    assert_eq!(
        task["status"]["state"], "submitted",
        "task state should be submitted; got: {response}"
    );

    let task_id = task["id"].as_str().unwrap().to_string();

    // Wait for agent to complete
    let launched = handle.join().expect("launch thread should not panic");

    // Agent ran and produced output
    let result = fs::read_to_string(launched.workdir.join("out/result.txt"))
        .expect("result.txt should exist");
    assert!(
        result.contains("A2A task complete"),
        "agent should have responded with the end_turn text; got: {result}"
    );

    // Server is shut down by now; can't query tasks/get after completion.
    // Verify via trace.jsonl instead.
    let trace_content =
        fs::read_to_string(launched.workdir.join("trace.jsonl")).unwrap_or_default();
    assert!(
        trace_content.contains("a2a_task_received"),
        "trace should contain a2a_task_received event; got:\n{trace_content}"
    );
    assert!(
        trace_content.contains(&task_id),
        "trace should contain the task_id; got:\n{trace_content}"
    );
}

#[test]
fn a2a_second_message_send_is_rejected() {
    // Use a two-response server so the session takes at least two round-trips,
    // giving us time to send a second message/send while the loop is running.
    let server = common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(&home, &manifest_path);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let first_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "msg-first",
                "role": "user",
                "parts": [{"text": "first task"}]
            }
        }
    })
    .to_string();

    let first_response = http_post_json(&capsule_url, "/", &first_body);
    assert_eq!(
        first_response["result"]["status"]["state"], "submitted",
        "first message/send should be submitted; got: {first_response}"
    );

    // Second message/send must be rejected
    let second_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "msg-second",
                "role": "user",
                "parts": [{"text": "should be rejected"}]
            }
        }
    })
    .to_string();

    let second_response = http_post_json(&capsule_url, "/", &second_body);
    assert_eq!(
        second_response["result"]["status"]["state"], "rejected",
        "second message/send should be rejected; got: {second_response}"
    );

    handle.join().expect("launch thread should not panic");
}

#[test]
fn a2a_task_md_fallback_not_regressed() {
    // When task.md exists, the capsule should run normally without waiting for A2A.
    let server = end_turn_server("fallback result");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(&home, &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run the fallback task").unwrap();

    let launched = launch_session(staged, |_| {}).expect("launch with task.md should succeed");

    let result = fs::read_to_string(launched.workdir.join("out/result.txt"))
        .expect("result.txt should exist");
    assert!(
        result.contains("fallback result"),
        "agent should have responded with end_turn text; got: {result}"
    );
}

#[test]
fn a2a_tasks_get_unknown_method_returns_error() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(&home, &manifest_path);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .unwrap()
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "unknown/method",
        "params": {}
    })
    .to_string();

    let response = http_post_json(&capsule_url, "/", &body);
    assert!(
        !response["error"].is_null(),
        "unknown method should return JSON-RPC error; got: {response}"
    );
    assert_eq!(
        response["error"]["code"], -32601,
        "error code should be -32601 (Method not found); got: {response}"
    );

    // Unblock the capsule by sending a proper message
    let msg_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": "unblock",
                "role": "user",
                "parts": [{"text": "proceed"}]
            }
        }
    })
    .to_string();
    let _ = http_post_json(&capsule_url, "/", &msg_body);

    handle.join().expect("launch thread should not panic");
}
