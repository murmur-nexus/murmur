//! Integration tests for input-required task state.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    LifecycleConfig, RuntimeError, StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, AfterTask, ArtifactRuntime, ContainmentClass, LocalRegistry,
    TaskAcceptance,
};
use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const TOOL_NAME: &str = "request-input-tool";
const TOOL_VERSION: &str = "0.1.0";

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn tool_wasm_path() -> PathBuf {
    fixture_path("input-required/tool/request-input-tool.wasm")
}

/// ScriptedServer that returns: first a tool_use call for request-input-tool,
/// then (after tool result) an end_turn response.
fn tool_then_end_turn_server(tool_input_data: &str, final_text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": TOOL_NAME,
                "input": { "data": tool_input_data }
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        serde_json::json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": final_text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ])
}

fn create_tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let opts: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", opts).unwrap();
    writeln!(zip, "name: {TOOL_NAME}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", opts).unwrap();
    zip.write_all(&fs::read(tool_wasm_path()).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

fn setup_project(home: &TempDir, endpoint: &str, extra_lifecycle_yaml: &str) -> (TempDir, PathBuf) {
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver_artifact).success();

    let tool_artifact = create_tool_artifact(artifacts.path());
    common::publish_local(home, &tool_artifact).success();

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: input-required-agent\n\
             version: 0.1.0\n\
             artifacts:\n\
             \x20 - name: {DRIVER_NAME}\n\
             \x20   version: {DRIVER_VERSION}\n\
             \x20   runtime: driver\n\
             \x20   gateway:\n\
             \x20     endpoint: {endpoint}\n\
             \x20     api_key: test-key\n\
             \x20 - name: {TOOL_NAME}\n\
             \x20   version: {TOOL_VERSION}\n\
             \x20   runtime: tool\n\
             capabilities:\n\
             \x20 network:\n\
             \x20   allow:\n\
             \x20     - {endpoint}\n\
             inference:\n\
             \x20 transport: http\n\
             \x20 model: test-model\n\
             \x20 driver:\n\
             \x20   artifact: {DRIVER_NAME}\n\
             {extra_lifecycle_yaml}"
        ),
    )
    .unwrap();

    (artifacts, project.keep().join("murmur.yaml"))
}

fn stage_agent(
    home: &TempDir,
    manifest_path: &Path,
    lifecycle: Option<LifecycleConfig>,
) -> capsule_runtime::StagedSession {
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
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_peers: None,
            ignore_task_file: false,
        },
    )
    .unwrap()
}

fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
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
    let mut body_str = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        body_str.push_str(&line);
    }
    serde_json::from_str(&body_str).unwrap_or_else(|_| serde_json::json!({"_raw": body_str}))
}

fn send_message(addr: &str, msg_id: &str, text: &str) -> Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": msg_id,
                "role": "user",
                "parts": [{"text": text}]
            }
        }
    })
    .to_string();
    http_post_json(addr, "/", &body)
}

fn tasks_get(addr: &str, task_id: &str) -> Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tasks/get",
        "params": {"id": task_id}
    })
    .to_string();
    http_post_json(addr, "/", &body)
}

/// Poll `tasks/get` with no `id` param — which returns whichever task holds the active
/// slot — until a task exists, and return its id.
///
/// Lets a caller that submitted a task over `message/stream` learn the server-assigned
/// task id, which that method never reports back over the wire.
fn discover_active_task_id(addr: &str, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tasks/get",
            "params": {}
        })
        .to_string();
        let resp = http_post_json(addr, "/", &body);
        if let Some(task_id) = resp["result"]["id"].as_str() {
            return task_id.to_string();
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out discovering the active task id; last response: {resp}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll tasks/get until the task reaches the expected state, or timeout.
fn poll_until_state(addr: &str, task_id: &str, expected_state: &str, timeout: Duration) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let resp = tasks_get(addr, task_id);
        let state = resp["result"]["status"]["state"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if state == expected_state {
            return resp;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for state '{expected_state}'; last response: {resp}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── SSE client helper ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SseEvent {
    id: Option<u64>,
    event_type: String,
    data: String,
}

/// Read one line, bounding the socket read by whatever is left of `deadline`.
///
/// Returns `None` once the deadline has passed, the peer closed the stream, or the read
/// failed. Arming the socket against the remaining budget rather than a fixed per-read
/// timeout is what makes the deadline cover the whole collection: the capsule sends a
/// `:heartbeat` comment every 15s, so any per-read timeout longer than that interval is
/// re-armed forever by traffic that carries no event.
fn read_line_before(
    reader: &mut BufReader<&TcpStream>,
    stream: &TcpStream,
    deadline: std::time::Instant,
) -> Option<String> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return None;
    }
    // A zero timeout means "block forever" to the sockets API, so never pass one.
    stream
        .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
        .ok()?;

    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

/// Subscribe to `message/stream` for a new task and collect SSE events until a terminal
/// `status` event arrives or `timeout` elapses. `timeout` bounds the whole collection,
/// not each read; on expiry the events gathered so far are returned rather than panicking.
fn collect_sse_events_for_message(
    addr: &str,
    msg_id: &str,
    text: &str,
    timeout: Duration,
) -> Vec<SseEvent> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {
            "message": {
                "messageId": msg_id,
                "role": "user",
                "parts": [{"text": text}]
            }
        }
    })
    .to_string();

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    );

    let deadline = std::time::Instant::now() + timeout;
    let stream = TcpStream::connect(addr).expect("should connect for SSE");

    {
        let mut w = &stream;
        w.write_all(request.as_bytes()).unwrap();
        let _ = w.flush();
    }

    let mut reader = BufReader::new(&stream);

    // Read status line
    let _ = read_line_before(&mut reader, &stream, deadline);

    // Skip headers
    while let Some(line) = read_line_before(&mut reader, &stream, deadline) {
        if line.trim().is_empty() {
            break;
        }
    }

    let mut events = Vec::new();
    let mut cur_id = None;
    let mut cur_type = String::new();
    let mut cur_data = String::new();

    while let Some(line) = read_line_before(&mut reader, &stream, deadline) {
        let line = line
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();

        if line.is_empty() {
            if !cur_type.is_empty() && !cur_data.is_empty() {
                let is_final = cur_type == "status" && cur_data.contains("\"final\":true");
                events.push(SseEvent {
                    id: cur_id,
                    event_type: cur_type.clone(),
                    data: cur_data.clone(),
                });
                if is_final {
                    break;
                }
            }
            cur_id = None;
            cur_type.clear();
            cur_data.clear();
        } else if let Some(rest) = line.strip_prefix("id: ") {
            cur_id = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            cur_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            cur_data = rest.to_string();
        }
    }

    events
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Test 1: A task that calls request-input suspends the agent loop and transitions
/// to input-required state with the prompt stored as an artifact.
#[test]
fn input_required_task_suspends_loop() {
    let server = tool_then_end_turn_server("What branch should I use?", "done");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");
    let staged = stage_agent(&home, &manifest_path, None);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let resp = send_message(&capsule_url, "msg-1", "start the task");
    assert_eq!(
        resp["result"]["status"]["state"], "submitted",
        "initial response should be submitted; got: {resp}"
    );
    let task_id = resp["result"]["id"].as_str().unwrap().to_string();

    // Wait for the task to enter input-required state
    let ir_resp = poll_until_state(
        &capsule_url,
        &task_id,
        "input-required",
        Duration::from_secs(30),
    );

    let artifacts = &ir_resp["result"]["artifacts"];
    assert!(
        artifacts.is_array(),
        "input-required task should have artifacts; got: {ir_resp}"
    );
    let prompt = artifacts[0]["parts"][0]["text"].as_str().unwrap_or("");
    assert!(
        prompt.contains("What branch"),
        "artifact should contain prompt text; got prompt: '{prompt}'"
    );

    // Unblock: deliver input to complete the task
    let _ = send_message(&capsule_url, "msg-2", "use main branch");

    handle.join().expect("launch thread should not panic");
}

/// Test 2: Delivering input via message/send resumes the suspended task and
/// the task eventually reaches completed state.
#[test]
fn input_required_resumes_on_message_send() {
    let server = tool_then_end_turn_server("Which option?", "task completed after input");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");
    let staged = stage_agent(&home, &manifest_path, None);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let resp = send_message(&capsule_url, "msg-1", "start task");
    let task_id = resp["result"]["id"].as_str().unwrap().to_string();

    // Wait for input-required
    poll_until_state(
        &capsule_url,
        &task_id,
        "input-required",
        Duration::from_secs(30),
    );

    // Deliver input: the second message/send should be routed to the waiting task
    let resume_resp = send_message(&capsule_url, "msg-2", "option A");
    let resume_state = resume_resp["result"]["status"]["state"]
        .as_str()
        .unwrap_or("");
    assert!(
        resume_state == "working" || resume_state == "completed",
        "delivering input should return working or completed; got: '{resume_state}' in {resume_resp}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Test 3: A message/send while the task is in working (not input-required) state
/// is rejected — the standard single-task rejection still applies.
#[test]
fn input_required_working_state_rejects_message() {
    // Use a slow server with two turns so the agent stays in working state long enough
    let server = common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done quickly"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");
    let staged = stage_agent(&home, &manifest_path, None);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    // First message starts the task
    let resp1 = send_message(&capsule_url, "msg-1", "first task");
    assert_eq!(
        resp1["result"]["status"]["state"], "submitted",
        "first message should be submitted; got: {resp1}"
    );

    // Second message while working — must be rejected
    let resp2 = send_message(&capsule_url, "msg-2", "concurrent task attempt");
    assert_eq!(
        resp2["result"]["status"]["state"], "rejected",
        "second message to working task should be rejected; got: {resp2}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Test 4: When input_timeout_secs elapses with no response, the task is `failed` on
/// `tasks/get`, the attempt ends without asking the provider for another turn, and the trace
/// says why. A queue capsule that sleeps between tasks keeps the door up to read.
#[test]
fn input_required_timeout_transitions_to_failed() {
    let server = tool_then_end_turn_server("Quick question", "would have completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");

    let lifecycle = LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: AfterTask::Sleep,
        input_timeout_secs: Some(2),
        ..Default::default()
    };
    let staged = stage_agent(&home, &manifest_path, Some(lifecycle));
    let trace_path = staged.workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let resp = send_message(&capsule_url, "msg-1", "start timed task");
    let task_id = resp["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(
        &capsule_url,
        &task_id,
        "input-required",
        Duration::from_secs(30),
    );
    poll_until_state(&capsule_url, &task_id, "failed", Duration::from_secs(30));

    let trace = wait_for_task_end(&trace_path, &task_id);
    assert_input_timeout_recorded(&trace, &task_id);
    assert_eq!(
        server.requests().len(),
        1,
        "the timed-out attempt asked the provider for no further turn"
    );
}

/// A streaming client of a task whose `request-input` wait times out is told once that the task
/// failed: after `input-required`, one final `failed` status with message `input-timeout`, the
/// last frame the connection carries, with no earlier `failed` frame.
#[test]
fn input_timeout_streams_one_failed_final_status() {
    let server = tool_then_end_turn_server("Quick question", "would have completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");

    let lifecycle = LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: AfterTask::Sleep,
        input_timeout_secs: Some(2),
        ..Default::default()
    };
    let staged = stage_agent(&home, &manifest_path, Some(lifecycle));

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let events = collect_sse_events_for_message(
        &capsule_url,
        "msg-stream-timeout",
        "start timed task",
        Duration::from_secs(60),
    );
    let statuses: Vec<Value> = events
        .iter()
        .filter(|e| e.event_type == "status")
        .map(|e| serde_json::from_str(&e.data).expect("status data is JSON"))
        .collect();
    assert!(
        statuses
            .iter()
            .any(|s| s["status"]["state"] == "input-required"),
        "{events:?}"
    );
    let failed: Vec<&Value> = statuses
        .iter()
        .filter(|s| s["status"]["state"] == "failed")
        .collect();
    assert_eq!(failed.len(), 1, "one failed status: {events:?}");
    assert_eq!(failed[0]["final"], true);
    assert_eq!(failed[0]["status"]["message"], "input-timeout");
    assert!(failed[0]["context_id"].is_string(), "{}", failed[0]);
    assert_eq!(
        statuses.last(),
        Some(failed[0]),
        "the final status is the last status: {events:?}"
    );
    assert_eq!(
        events.last().map(|e| e.event_type.as_str()),
        Some("status"),
        "{events:?}"
    );

    let task_id = failed[0]["id"].as_str().unwrap();
    assert_eq!(
        tasks_get(&capsule_url, task_id)["result"]["status"]["state"],
        "failed"
    );
}

/// The same timeout on a queue capsule that exits after its task: the launch reports the task
/// `failed`, naming the timeout, and the provider was asked once.
#[test]
fn input_timeout_fails_the_launch() {
    let server = tool_then_end_turn_server("Quick question", "would have completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");

    let lifecycle = LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: AfterTask::Exit,
        input_timeout_secs: Some(2),
        ..Default::default()
    };
    let staged = stage_agent(&home, &manifest_path, Some(lifecycle));
    let trace_path = staged.workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
    });
    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let resp = send_message(&capsule_url, "msg-1", "start timed task");
    let task_id = resp["result"]["id"].as_str().unwrap().to_string();

    match handle.join().expect("launch thread should not panic") {
        Err(RuntimeError::TaskDidNotComplete {
            exit_status,
            reason,
        }) => {
            assert_eq!(exit_status, "failed", "{reason}");
            assert!(reason.contains("lifecycle.input_timeout_secs"), "{reason}");
        }
        other => panic!("expected TaskDidNotComplete(failed), got {other:?}"),
    }
    let trace = read_trace(&trace_path);
    assert_input_timeout_recorded(&trace, &task_id);
    assert_eq!(server.requests().len(), 1);
}

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

/// The trace once it holds `task_id`'s `task_end`.
fn wait_for_task_end(path: &Path, task_id: &str) -> Vec<Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let trace = read_trace(path);
        if trace
            .iter()
            .any(|event| event["event_type"] == "task_end" && event["task_id"] == task_id)
        {
            return trace;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no task_end for {task_id}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One `task_failed{cause: input_timeout}` for `task_id`, before its `task_end failed`.
fn assert_input_timeout_recorded(trace: &[Value], task_id: &str) {
    let position = |event_type: &str| {
        trace
            .iter()
            .position(|event| event["event_type"] == event_type && event["task_id"] == task_id)
            .unwrap_or_else(|| panic!("no {event_type} for {task_id}: {trace:?}"))
    };
    let failed: Vec<_> = trace
        .iter()
        .filter(|event| event["event_type"] == "task_failed" && event["task_id"] == task_id)
        .collect();
    assert_eq!(failed.len(), 1, "{trace:?}");
    assert_eq!(failed[0]["cause"], "input_timeout");
    assert!(position("task_failed") < position("task_end"));
    assert_eq!(trace[position("task_end")]["exit_status"], "failed");
}

/// Test 5: message/stream SSE stream receives an input-required status event
/// (with final:false), and after delivering input via message/send the stream
/// eventually sees a completed final event.
#[test]
fn input_required_sse_emits_state_event() {
    let server = tool_then_end_turn_server("SSE branch?", "stream completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");
    let staged = stage_agent(&home, &manifest_path, None);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    // Spawn SSE collection in background — it will block until final event or timeout
    let addr_clone = capsule_url.clone();
    let sse_handle = std::thread::spawn(move || {
        collect_sse_events_for_message(
            &addr_clone,
            "msg-sse-1",
            "stream task start",
            Duration::from_secs(60),
        )
    });

    // message/stream never reports the task id, so read it off the active slot.
    let task_id = discover_active_task_id(&capsule_url, Duration::from_secs(30));

    // Input may only be delivered once the task has actually suspended: a message/send
    // that lands while the task is still working is rejected as a concurrent task, and
    // the suspended task then waits for input that never arrives.
    poll_until_state(
        &capsule_url,
        &task_id,
        "input-required",
        Duration::from_secs(30),
    );

    let resume_resp = send_message(&capsule_url, "msg-sse-2", "use feature branch");
    let resume_state = resume_resp["result"]["status"]["state"]
        .as_str()
        .unwrap_or("");
    assert!(
        resume_state == "working" || resume_state == "completed",
        "delivering input should return working or completed; got: '{resume_state}' in {resume_resp}"
    );

    let events = sse_handle
        .join()
        .expect("SSE collection thread should not panic");
    for event in &events {
        eprintln!("event: {}\ndata: {}\n", event.event_type, event.data);
    }

    assert!(
        !events.is_empty(),
        "should have received SSE events; got none"
    );

    let input_required_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "status" && e.data.contains("input-required"))
        .collect();
    assert!(
        !input_required_events.is_empty(),
        "should have an input-required status SSE event; got events: {events:?}"
    );

    // The input-required event must have final:false
    let ir_event = &input_required_events[0];
    assert!(
        !ir_event.data.contains("\"final\":true"),
        "input-required SSE event should have final:false; got: {}",
        ir_event.data
    );

    let final_event = events
        .iter()
        .find(|e| e.event_type == "status" && e.data.contains("\"final\":true"));
    assert!(
        final_event.is_some(),
        "should have a final SSE event; got events: {events:?}"
    );
    assert!(
        final_event.unwrap().data.contains("\"completed\""),
        "final SSE event should be completed; got: {}",
        final_event.unwrap().data
    );

    // Every frame on the stream — the agent loop's, the `request-input` wait's `input-required`
    // and `resumed`, the chunks and the final status — is numbered from the session's one
    // sequence, which starts at 1 and rises by one per frame.
    let ids: Vec<u64> = events
        .iter()
        .map(|e| {
            e.id.unwrap_or_else(|| panic!("frame without an id on the stream: {e:?}"))
        })
        .collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "event ids should strictly ascend across every frame kind; got {ids:?}"
    );
    assert!(
        ids.iter().all(|id| *id < 1 << 32),
        "event ids should come from one sequence starting at 1; got {ids:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "status" && e.data.contains("\"resumed\"")),
        "the stream should carry the resumed status; got events: {events:?}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Test 6: `collect_sse_events_for_message` gives up at its deadline even while the
/// stream stays alive. A task that suspends on request-input with no input delivered
/// emits no terminal event and keeps heartbeating, so a reader that bounded only each
/// individual read would never return.
#[test]
fn sse_reader_returns_when_deadline_exceeded() {
    let server = tool_then_end_turn_server("Deadline branch?", "would have completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");
    let staged = stage_agent(&home, &manifest_path, None);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule URL");

    let budget = Duration::from_secs(5);
    let started = std::time::Instant::now();
    let events =
        collect_sse_events_for_message(&capsule_url, "msg-deadline-1", "stream task start", budget);
    let elapsed = started.elapsed();

    assert!(
        elapsed >= budget,
        "collection should run for the full budget before giving up; returned after {elapsed:?}"
    );
    assert!(
        elapsed < budget + Duration::from_secs(5),
        "collection should return at its deadline, not block on the live stream; took {elapsed:?}"
    );

    assert!(
        !events.is_empty(),
        "stream should have delivered events before the deadline; got none"
    );
    assert!(
        events
            .iter()
            .any(|e| e.event_type == "status" && e.data.contains("input-required")),
        "stream should have delivered the input-required event; got events: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.data.contains("\"final\":true")),
        "no terminal event is emitted while the task waits for input; got events: {events:?}"
    );

    // Release the suspended task so the capsule can shut down.
    let task_id = discover_active_task_id(&capsule_url, Duration::from_secs(30));
    poll_until_state(
        &capsule_url,
        &task_id,
        "input-required",
        Duration::from_secs(30),
    );
    let _ = send_message(&capsule_url, "msg-deadline-2", "use main branch");

    handle.join().expect("launch thread should not panic");
}
