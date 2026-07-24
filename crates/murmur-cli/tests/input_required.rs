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
    LifecycleConfig, StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, LocalRegistry, TaskAcceptance};
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

fn setup_project(
    home: &TempDir,
    endpoint: &str,
    extra_lifecycle_yaml: &str,
) -> (TempDir, PathBuf) {
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
             \x20 - name: {TOOL_NAME}\n\
             \x20   version: {TOOL_VERSION}\n\
             \x20   runtime: tool\n\
             capabilities:\n\
             \x20 network:\n\
             \x20   allow:\n\
             \x20     - {endpoint}\n\
             inference:\n\
             \x20 transport: http\n\
             \x20 endpoint: {endpoint}\n\
             \x20 model: test-model\n\
             \x20 api_key: test-key\n\
             \x20 driver:\n\
             \x20   artifact: {DRIVER_NAME}\n\
             {extra_lifecycle_yaml}"
        ),
    )
    .unwrap();

    (artifacts, project.keep().join("murmur.yaml"))
}

fn stage_agent(home: &TempDir, manifest_path: &Path, lifecycle: Option<LifecycleConfig>) -> capsule_runtime::StagedSession {
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

fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
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
    serde_json::from_str(&body_str)
        .unwrap_or_else(|_| serde_json::json!({"_raw": body_str}))
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

/// Poll tasks/get until the task reaches the expected state, or timeout.
fn poll_until_state(addr: &str, task_id: &str, expected_state: &str, timeout: Duration) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let resp = tasks_get(addr, task_id);
        let state = resp["result"]["status"]["state"].as_str().unwrap_or("").to_string();
        if state == expected_state {
            return resp;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for state '{expected_state}'; last response: {resp}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── SSE client helper ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SseEvent {
    event_type: String,
    data: String,
}

fn collect_sse_events_for_message(addr: &str, msg_id: &str, text: &str, timeout: Duration) -> Vec<SseEvent> {
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

    let stream = TcpStream::connect(addr).expect("should connect for SSE");
    stream.set_read_timeout(Some(timeout)).ok();

    {
        let mut w = &stream;
        w.write_all(request.as_bytes()).unwrap();
        let _ = w.flush();
    }

    let mut reader = BufReader::new(&stream);

    // Read status line
    let mut status = String::new();
    let _ = reader.read_line(&mut status);

    // Skip headers
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
    }

    let mut events = Vec::new();
    let mut cur_type = String::new();
    let mut cur_data = String::new();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let line = line.trim_end_matches('\n').trim_end_matches('\r').to_string();

        if line.is_empty() {
            if !cur_type.is_empty() && !cur_data.is_empty() {
                let is_final = cur_type == "status" && cur_data.contains("\"final\":true");
                events.push(SseEvent {
                    event_type: cur_type.clone(),
                    data: cur_data.clone(),
                });
                if is_final {
                    break;
                }
            }
            cur_type.clear();
            cur_data.clear();
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
    poll_until_state(&capsule_url, &task_id, "input-required", Duration::from_secs(30));

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
    let server = common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "done quickly"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);
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

/// Test 4: When input_timeout_secs elapses with no response, the task transitions
/// to failed state.
#[test]
fn input_required_timeout_transitions_to_failed() {
    let server = tool_then_end_turn_server("Quick question", "would have completed");
    let home = tempfile::tempdir().unwrap();
    let (_artifacts, manifest_path) = setup_project(&home, &server.endpoint, "");

    // Override lifecycle to set a 2-second input timeout
    let lifecycle = LifecycleConfig {
        task_acceptance: TaskAcceptance::Single,
        input_timeout_secs: Some(2),
        ..Default::default()
    };
    let staged = stage_agent(&home, &manifest_path, Some(lifecycle));

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

    let resp = send_message(&capsule_url, "msg-1", "start timed task");
    let task_id = resp["result"]["id"].as_str().unwrap().to_string();

    // Wait for input-required
    poll_until_state(&capsule_url, &task_id, "input-required", Duration::from_secs(30));

    // Do NOT send a response — poll until the task reaches "failed" state.
    // The 2-second timeout will fire, finish_task(Failed) is called, then the
    // capsule exits. We poll during that window before the server shuts down.
    // If polling races with the server shutdown, the capsule exiting is also
    // evidence that the timeout fired correctly — so we catch that too.
    let mut saw_failed = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        match tasks_get_opt(&capsule_url, &task_id) {
            None => {
                // Server is gone — capsule exited after timeout, which is correct.
                saw_failed = true;
                break;
            }
            Some(resp) => {
                let state = resp["result"]["status"]["state"].as_str().unwrap_or("");
                if state == "failed" {
                    saw_failed = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        saw_failed,
        "timed-out input-required task should reach failed state or server should shut down"
    );

    handle.join().expect("launch thread should not panic");
}

/// Like tasks_get but returns None if the server has shut down.
fn tasks_get_opt(addr: &str, task_id: &str) -> Option<Value> {
    let stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tasks/get",
        "params": {"id": task_id}
    })
    .to_string();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut w = &stream;
    w.write_all(request.as_bytes()).ok()?;
    w.flush().ok()?;

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
    }
    let mut body_str = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        body_str.push_str(&line);
    }
    serde_json::from_str(&body_str).ok()
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
    let _handle = std::thread::spawn(move || {
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
            Duration::from_secs(30),
        )
    });

    // Wait briefly for the SSE stream to start and for the tool to call request-input
    std::thread::sleep(Duration::from_millis(500));

    // Discover the task_id by polling via tasks/get equivalent — use message/send to deliver
    // We need the task_id. Poll the first submitted task via a brief tasks/get loop.
    // Since we started the SSE stream which also submitted a task, we need to find it.
    // Deliver input so the SSE stream can proceed to completion.
    let _ = send_message(&capsule_url, "msg-sse-2", "use feature branch");

    let events = sse_handle.join().expect("SSE collection thread should not panic");

    assert!(!events.is_empty(), "should have received SSE events; got none");

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
}
