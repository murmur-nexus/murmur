#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, LifecycleOverride, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

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

fn multi_turn_server(texts: &[&str]) -> common::ScriptedServer {
    let responses = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            serde_json::json!({
                "id": format!("msg_{}", i + 1),
                "type": "message",
                "role": "assistant",
                "model": "test-model",
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string()
        })
        .collect();
    common::ScriptedServer::start(responses)
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
            "name: lifecycle-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(
    home: &TempDir,
    manifest_path: &Path,
    lifecycle: Option<LifecycleConfig>,
    lifecycle_override: Option<LifecycleOverride>,
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
            lifecycle,
            lifecycle_override,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            job_id: None,
        },
    )
    .unwrap()
}

fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(&stream);
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

fn message_send_body(message_id: &str, text: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": message_id,
                "role": "user",
                "parts": [{"text": text}]
            }
        }
    })
    .to_string()
}

/// Capsule with task_acceptance: none runs from task.md but HTTP message/send returns -32601.
#[test]
fn lifecycle_none_rejects_message_send() {
    let server = end_turn_server("none mode done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::None,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );
    // Write task.md so the agent loop runs (giving us a window to send HTTP requests)
    fs::write(staged.workdir.join("task.md"), "Run the task").unwrap();

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

    let response = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("m-none", "should be rejected"),
    );
    assert_eq!(
        response["error"]["code"], -32601,
        "task_acceptance: none should reject message/send with -32601; got: {response}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Capsule with queue+sleep processes two queued tasks and stays alive indefinitely.
/// Termination is external (channel drop); idle timeout must NOT fire in this mode.
#[test]
fn lifecycle_queue_sleep_processes_two_tasks() {
    let server = multi_turn_server(&["task one done", "task two done"]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 2,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );
    let workdir_for_thread = staged.workdir.clone();

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

    // Enqueue both tasks before the agent processes either
    let r1 = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-1", "first task"),
    );
    assert_eq!(
        r1["result"]["status"]["state"], "submitted",
        "first task should be submitted; got: {r1}"
    );

    let r2 = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-2", "second task"),
    );
    assert_eq!(
        r2["result"]["status"]["state"], "submitted",
        "second task should be submitted; got: {r2}"
    );

    // Poll the trace file until both task_end events appear (max 30 s).
    // Do NOT rely on idle timeout — queue+sleep capsules wait indefinitely.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let trace = fs::read_to_string(workdir_for_thread.join("trace.jsonl")).unwrap_or_default();
        let task_end_count = trace.lines().filter(|l| l.contains("\"task_end\"")).count();
        if task_end_count >= 2 {
            let received_count = trace
                .lines()
                .filter(|l| l.contains("a2a_task_received"))
                .count();
            assert_eq!(
                received_count, 2,
                "trace should record 2 a2a_task_received events; got:\n{trace}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for both tasks to complete; trace:\n{}",
            fs::read_to_string(workdir_for_thread.join("trace.jsonl")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // The capsule is still alive (queue+sleep). Drop the join handle without waiting.
    drop(handle);
}

/// LifecycleOverride forces task_acceptance: none regardless of manifest default.
#[test]
fn lifecycle_override_forces_none() {
    let server = end_turn_server("override done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    // Manifest has no lifecycle (defaults to single+exit); override forces none
    let staged = stage_agent(
        &home,
        &manifest_path,
        None,
        Some(LifecycleOverride {
            task_acceptance: Some(TaskAcceptance::None),
            after_task: None,
        }),
    );
    fs::write(staged.workdir.join("task.md"), "Override task").unwrap();

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

    let response = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("m-override", "should be rejected by override"),
    );
    assert_eq!(
        response["error"]["code"], -32601,
        "overridden-to-none lifecycle should reject message/send with -32601; got: {response}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Capsule with task_acceptance: none and no task.md exits immediately (no 30-second wait).
#[test]
fn lifecycle_none_exits_immediately_without_input() {
    // Server with no responses — the capsule must not make any inference calls
    let _server = common::ScriptedServer::start(vec![]);
    let (home, manifest_path) = setup_agent_project(&_server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::None,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );

    // No task.md written — capsule must exit without waiting

    let start = std::time::Instant::now();
    launch_session(staged, |_| {}).expect("launch should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "task_acceptance: none with no task.md should exit in < 5s (got {elapsed:?})"
    );
}
