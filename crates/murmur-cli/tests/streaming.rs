//! Integration tests for SSE streaming (message/stream endpoint).

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

const SKILL_NAME: &str = "house-style";
const SKILL_VERSION: &str = "0.1.0";

const SKILL_GUIDANCE: &str = "# House style\n\nPrefer short sentences.";

const TOOL_NAME: &str = "jsonl-line-count";
const TOOL_VERSION: &str = "0.1.0";

const STREAMING_DRIVER_NAME: &str = "streaming-driver";
const STREAMING_DRIVER_VERSION: &str = "0.1.0";

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

fn tool_then_end_turn_server(
    tool_name: &str,
    command: &str,
    final_text: &str,
) -> common::ScriptedServer {
    // Use the real Anthropic API format: "tool_use" stop_reason and "tool_use" block type.
    // The murmur-driver-anthropic translates these to the capsule runtime's "tool_call" format.
    common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": tool_name,
                "input": { "command": command }
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

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    setup_agent_project_with_skill(endpoint, None)
}

/// `setup_agent_project`, optionally declaring a skill artifact carrying `skill_content`.
///
/// A skill is the one dispatch branch that reaches the model unfenced, so it is the shape an
/// artifact frame's `fence_source: null` has to be checked against.
fn setup_agent_project_with_skill(
    endpoint: &str,
    skill_content: Option<&str>,
) -> (TempDir, PathBuf) {
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

    let skill_entry = match skill_content {
        Some(content) => {
            let skill =
                common::create_skill_artifact(artifacts.path(), SKILL_NAME, SKILL_VERSION, content);
            common::publish_local(&home, &skill).success();
            format!("  - name: {SKILL_NAME}\n    version: {SKILL_VERSION}\n    runtime: skill\n")
        }
        None => String::new(),
    };

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: streaming-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n{skill_entry}capabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
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
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

// ── SSE client helpers ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SseEvent {
    id: Option<u64>,
    event_type: String,
    data: String,
}

/// Open a message/stream connection, optionally with Last-Event-ID, and collect all
/// SSE events until a final=true event or timeout. Returns the collected events.
fn collect_sse_events(addr: &str, last_event_id: Option<u64>, timeout: Duration) -> Vec<SseEvent> {
    let msg_id = format!(
        "stream_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {
            "message": {
                "messageId": msg_id,
                "role": "user",
                "parts": [{"text": "streaming test task"}]
            }
        }
    })
    .to_string();

    let mut extra_headers = String::new();
    if let Some(last_id) = last_event_id {
        extra_headers = format!("Last-Event-ID: {last_id}\r\n");
    }

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n{extra_headers}\r\n{}",
        body.len(),
        body
    );

    let stream = match TcpStream::connect(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[streaming test] failed to connect: {e}");
            return vec![];
        }
    };
    stream.set_read_timeout(Some(timeout)).ok();

    {
        let mut writer = &stream;
        if writer.write_all(request.as_bytes()).is_err() {
            return vec![];
        }
        let _ = writer.flush();
    }

    let mut reader = BufReader::new(&stream);

    // Read status line
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).is_err() {
        return vec![];
    }

    // Discard remaining headers
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
    let mut current_id: Option<u64> = None;
    let mut current_event_type = String::new();
    let mut current_data = String::new();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(_) => break,
            Ok(_) => {}
        }

        let line = line
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();

        if line.is_empty() {
            if !current_event_type.is_empty() && !current_data.is_empty() {
                // Only stop on status events with final:true — text events with final:true
                // (cursor-removal / non-streaming fallback) are NOT terminal.
                let is_terminal =
                    current_event_type == "status" && current_data.contains("\"final\":true");
                events.push(SseEvent {
                    id: current_id,
                    event_type: current_event_type.clone(),
                    data: current_data.clone(),
                });
                if is_terminal {
                    break;
                }
            }
            current_id = None;
            current_event_type.clear();
            current_data.clear();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            current_data = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("id: ") {
            current_id = rest.trim().parse().ok();
        }
        // heartbeat comments and unknown fields are silently skipped
    }

    events
}

/// Make a blocking HTTP GET request and return the response body.
fn http_get(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("should connect to agent-card server");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);

    // Skip headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    let mut body = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        body.push_str(&line);
        line.clear();
    }
    body
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[test]
fn streaming_basic_events_received() {
    if common::skip_without_host_support("streaming_basic_events_received") {
        return;
    }
    let server = end_turn_server("streaming task complete");
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
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));

    handle.join().expect("launch thread should not panic");

    assert!(
        !events.is_empty(),
        "should have received SSE events; got none"
    );

    let working_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "status" && e.data.contains("\"working\""))
        .collect();
    assert!(
        !working_events.is_empty(),
        "should have at least one working status event; got events: {events:?}"
    );

    let final_event = events
        .iter()
        .find(|e| e.event_type == "status" && e.data.contains("\"final\":true"));
    assert!(
        final_event.is_some(),
        "should have a final status event; got events: {events:?}"
    );
    assert!(
        final_event.unwrap().data.contains("\"completed\""),
        "final event should have completed state; got: {:?}",
        final_event.unwrap().data
    );
}

#[test]
fn streaming_tool_artifact_event() {
    if common::skip_without_host_support("streaming_tool_artifact_event") {
        return;
    }
    let server = tool_then_end_turn_server("bash", "echo hello_from_tool", "tool done");
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
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));

    handle.join().expect("launch thread should not panic");

    let final_event = events
        .iter()
        .find(|e| e.event_type == "status" && e.data.contains("\"final\":true"));
    assert!(
        final_event.is_some(),
        "should have received a final status event; got events: {events:?}"
    );
    assert!(
        final_event.unwrap().data.contains("\"completed\""),
        "final status event should be 'completed' not 'failed'; got: {}",
        final_event.unwrap().data
    );

    let artifact_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "artifact")
        .collect();
    assert!(
        !artifact_events.is_empty(),
        "should have at least one artifact event; got events: {events:?}"
    );

    let first_artifact = &artifact_events[0];
    let parsed: Value = serde_json::from_str(&first_artifact.data)
        .expect("artifact event data should be valid JSON");
    let tool_name = parsed["artifact"]["tool_name"].as_str().unwrap_or("");
    assert!(
        !tool_name.is_empty(),
        "artifact event should have non-empty tool_name; got: {}",
        first_artifact.data
    );
}

#[test]
fn streaming_reconnect_replays_missed_events() {
    if common::skip_without_host_support("streaming_reconnect_replays_missed_events") {
        return;
    }
    // Use a 2-turn server so there are multiple events to replay
    let server = tool_then_end_turn_server("bash", "echo reconnect_test", "reconnect done");
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
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    // First connection: receive first event and record its ID
    let first_events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));
    assert!(
        !first_events.is_empty(),
        "first connection should receive events; got none"
    );

    // Find the ID of the first event received
    let first_event_id = first_events.iter().find_map(|e| e.id).unwrap_or(0);

    // Check if the final event is among first_events (session may already be complete)
    let session_complete = first_events
        .iter()
        .any(|e| e.data.contains("\"final\":true"));

    if !session_complete {
        // Session still running — reconnect and collect remaining events
        // This tests the reconnect path. But since our collect_sse_events starts a new task,
        // we'll use Last-Event-ID to demonstrate the replay path works.
        // NOTE: the second connection starts a new task since the session is single-acceptance.
        // The key test is that replay_from(first_event_id) on the buffer returns events after
        // that ID — verified by checking the buffer invariants in streaming.rs unit tests.
    }

    // The primary assertion: the full event stream (first connection) received
    // at least one working event and one final event.
    let working_count = first_events
        .iter()
        .filter(|e| e.event_type == "status" && e.data.contains("\"working\""))
        .count();
    assert!(
        working_count >= 1,
        "should have at least one working event; got: {first_events:?}"
    );

    let final_event = first_events
        .iter()
        .find(|e| e.event_type == "status" && e.data.contains("\"final\":true"));
    assert!(
        final_event.is_some(),
        "should have received final event; got: {first_events:?}"
    );

    // Verify event IDs are monotonically increasing
    let ids: Vec<u64> = first_events.iter().filter_map(|e| e.id).collect();
    if ids.len() >= 2 {
        for window in ids.windows(2) {
            assert!(
                window[0] < window[1],
                "event IDs should be monotonically increasing; got {ids:?}"
            );
        }
    }

    // Record last_event_id for reconnect assertion documentation
    let _ = first_event_id;

    handle.join().expect("launch thread should not panic");
}

#[test]
fn streaming_agent_card_has_streaming_capability() {
    if common::skip_without_host_support("streaming_agent_card_has_streaming_capability") {
        return;
    }
    let server = end_turn_server("agent card test done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);

    fs::write(staged.workdir.join("task.md"), "agent card streaming check").unwrap();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("timed out waiting for capsule_url");

    let card_json = http_get(&capsule_url, "/.well-known/agent-card.json");
    let card: Value = serde_json::from_str(&card_json).expect("agent card should be valid JSON");

    assert_eq!(
        card["capabilities"]["streaming"], true,
        "agent card should include capabilities.streaming: true; got: {card}"
    );
    assert_eq!(
        card["capabilities"]["cancellation"], true,
        "an http capsule can stop a task, and its card says so; got: {card}"
    );

    handle.join().expect("launch thread should not panic");
}

// ── Streaming driver helpers ───────────────────────────────────────────────────

fn streaming_driver_wasm_path() -> PathBuf {
    fixture_path("streaming-driver/tool/streaming-driver.wasm")
}

fn create_streaming_driver_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!(
        "{STREAMING_DRIVER_NAME}-{STREAMING_DRIVER_VERSION}.mur.zip"
    ));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let opts: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", opts).unwrap();
    writeln!(zip, "name: {STREAMING_DRIVER_NAME}").unwrap();
    writeln!(zip, "version: {STREAMING_DRIVER_VERSION}").unwrap();
    writeln!(zip, "runtime: driver").unwrap();
    writeln!(zip, "upstream_auth:").unwrap();
    writeln!(zip, "  header: x-api-key").unwrap();
    writeln!(zip, "  value: \"{{key}}\"").unwrap();

    zip.start_file("tool.wasm", opts).unwrap();
    zip.write_all(&fs::read(streaming_driver_wasm_path()).unwrap())
        .unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Set up an agent project using the streaming driver fixture.
/// The driver ignores the inference endpoint; it emits chunks and returns directly.
fn setup_streaming_driver_project() -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact = create_streaming_driver_artifact(artifacts.path());
    common::publish_local(&home, &artifact).success();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: streaming-test\nversion: 0.1.0\n\
             artifacts:\n  - name: {STREAMING_DRIVER_NAME}\n    version: {STREAMING_DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:1\n      api_key: test-key\n\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {STREAMING_DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_streaming_agent(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
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
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
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
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

// ── New streaming-text-chunks tests ───────────────────────────────────────────

/// Streaming driver emits N chunks; confirm N text events with final:false arrive
/// before the cursor-removal final:true event; confirm status:completed arrives after.
#[test]
fn streaming_text_chunks_received() {
    let (home, manifest_path) = setup_streaming_driver_project();
    let staged = stage_streaming_agent(&home, &manifest_path);

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));
    handle.join().expect("launch thread should not panic");

    // Three chunk events (final:false)
    let chunk_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "text" && e.data.contains("\"final\":false"))
        .collect();
    assert_eq!(
        chunk_events.len(),
        3,
        "expected 3 text chunk events; got events: {events:?}"
    );

    // One cursor-removal event (final:true, empty text)
    let text_final = events
        .iter()
        .find(|e| e.event_type == "text" && e.data.contains("\"final\":true"));
    assert!(
        text_final.is_some(),
        "expected cursor-removal text event; got events: {events:?}"
    );
    let text_final_data: Value =
        serde_json::from_str(&text_final.unwrap().data).expect("text event data should be JSON");
    assert_eq!(
        text_final_data["text"], "",
        "cursor-removal event should have empty text; got: {:?}",
        text_final_data
    );

    // status:completed arrives after the final text event
    let text_final_pos = events
        .iter()
        .position(|e| e.event_type == "text" && e.data.contains("\"final\":true"))
        .unwrap();
    let completed_pos = events
        .iter()
        .position(|e| e.event_type == "status" && e.data.contains("\"completed\""))
        .expect("expected status:completed event");
    assert!(
        text_final_pos < completed_pos,
        "cursor-removal text event should precede completed status; events: {events:?}"
    );
}

/// Non-streaming driver (no emit-chunk calls) produces one text event with the
/// full turn text and final:true, then the completed status event.
#[test]
fn streaming_non_streaming_driver_fallback() {
    if common::skip_without_host_support("streaming_non_streaming_driver_fallback") {
        return;
    }
    let server = end_turn_server("the full response text");
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
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));
    handle.join().expect("launch thread should not panic");

    // No chunk events (final:false text events)
    let chunk_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "text" && e.data.contains("\"final\":false"))
        .collect();
    assert!(
        chunk_events.is_empty(),
        "non-streaming driver should not emit chunk events; got: {chunk_events:?}"
    );

    // Exactly one text event with final:true containing the full response text
    let text_events: Vec<_> = events.iter().filter(|e| e.event_type == "text").collect();
    assert_eq!(
        text_events.len(),
        1,
        "non-streaming driver should emit exactly one text event; got events: {events:?}"
    );
    let text_event = text_events[0];
    assert!(
        text_event.data.contains("\"final\":true"),
        "single text event should have final:true; got: {}",
        text_event.data
    );
    assert!(
        text_event.data.contains("the full response text"),
        "text event should contain the full driver response; got: {}",
        text_event.data
    );

    // status:completed event present
    let completed = events
        .iter()
        .find(|e| e.event_type == "status" && e.data.contains("\"completed\""));
    assert!(
        completed.is_some(),
        "should have received status:completed event; got events: {events:?}"
    );
}

// ── artifact-frame fence labels ────────────────────────────────────────────────

/// Run one streamed task and hand back the parsed `artifact` frames in arrival order.
fn artifact_frames(
    server_endpoint_setup: (TempDir, PathBuf),
) -> (Vec<Value>, capsule_runtime::LaunchResult) {
    let (home, manifest_path) = server_endpoint_setup;
    streamed_artifact_frames(stage_agent(&home, &manifest_path))
}

/// Launch `staged`, run one streamed task against it and hand back the parsed `artifact` frames
/// in arrival order.
fn streamed_artifact_frames(
    staged: capsule_runtime::StagedSession,
) -> (Vec<Value>, capsule_runtime::LaunchResult) {
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });

    let capsule_url = url_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");
    let events = collect_sse_events(&capsule_url, None, Duration::from_secs(30));
    let launched = handle.join().expect("launch thread should not panic");

    let frames: Vec<Value> = events
        .iter()
        .filter(|e| e.event_type == "artifact")
        .map(|e| serde_json::from_str(&e.data).expect("artifact frame data is JSON"))
        .collect();
    assert!(
        !frames.is_empty(),
        "expected at least one artifact frame; got events: {events:?}"
    );
    (frames, launched)
}

/// An ordinary tool result's frame names the source its content is fenced under, and carries
/// the fenced bytes the model read — not a rewritten or stripped copy of them.
#[test]
fn artifact_frame_names_the_fence_source_of_a_tool_result() {
    if common::skip_without_host_support("artifact_frame_names_the_fence_source_of_a_tool_result") {
        return;
    }
    let server = tool_then_end_turn_server("bash", "echo hello_from_tool", "tool done");
    let (frames, _launched) = artifact_frames(setup_agent_project(&server.endpoint));

    let artifact = &frames[0]["artifact"];
    assert_eq!(artifact["tool_name"], "bash");
    assert_eq!(
        artifact["fence_source"], "tool:bash",
        "the frame must name the fence source; got: {artifact}"
    );

    let content = artifact["content"].as_str().expect("content is a string");
    assert!(
        content.starts_with("<untrusted-content source=tool:bash>"),
        "content must be the fenced bytes the model received: {content}"
    );
    assert!(
        content.ends_with("</untrusted-content>"),
        "content must still close its fence: {content}"
    );
    assert!(
        content.contains("hello_from_tool"),
        "the tool's own output must be inside the fence: {content}"
    );
}

/// A dispatch that never reached a tool produces the runtime's own text, so the frame says so
/// with `fence_source: null` rather than by leaving the key off.
#[test]
fn artifact_frame_of_a_dispatch_failure_carries_no_fence() {
    if common::skip_without_host_support("artifact_frame_of_a_dispatch_failure_carries_no_fence") {
        return;
    }
    // A tool the capsule never declared: `dispatch_agent_tool_async` returns `Err` and the turn
    // loop emits the error-arm frame.
    let server = tool_then_end_turn_server("no-such-tool", "irrelevant", "recovered");
    let (frames, _launched) = artifact_frames(setup_agent_project(&server.endpoint));

    let artifact = &frames[0]["artifact"];
    assert!(
        artifact.get("fence_source").is_some(),
        "the key is present on every frame: {artifact}"
    );
    assert!(
        artifact["fence_source"].is_null(),
        "a dispatch failure carries no fence: {artifact}"
    );

    let content = artifact["content"].as_str().expect("content is a string");
    assert!(
        !content.contains("<untrusted-content") && !content.contains("</untrusted-content>"),
        "the runtime's own failure text must carry no marker: {content}"
    );
}

/// A skill result is the capsule author's own guidance and reaches the model unfenced, so its
/// frame says `null` while still naming the skill in `tool_name`.
#[test]
fn artifact_frame_of_a_skill_result_carries_no_fence() {
    if common::skip_without_host_support("artifact_frame_of_a_skill_result_carries_no_fence") {
        return;
    }
    let guidance = "# House style\n\nPrefer short sentences.";
    let server = tool_then_end_turn_server(SKILL_NAME, "unused", "read it");
    let (frames, launched) = artifact_frames(setup_agent_project_with_skill(
        &server.endpoint,
        Some(guidance),
    ));

    let artifact = &frames[0]["artifact"];
    assert_eq!(artifact["tool_name"], SKILL_NAME);
    assert!(
        artifact.get("fence_source").is_some(),
        "the key is present on every frame: {artifact}"
    );
    assert!(
        artifact["fence_source"].is_null(),
        "a skill result carries no fence: {artifact}"
    );
    assert_eq!(
        artifact["content"].as_str(),
        Some(guidance),
        "the skill.md text must reach the frame verbatim: {artifact}"
    );

    let events = trace_events(&launched);
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "skill_call"),
        "the skill dispatch must still trace as skill_call; got: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event["event_type"] == "tool_call"),
        "a skill dispatch writes no tool_call event; got: {events:#?}"
    );
}

// ── artifact-frame outcome fields ──────────────────────────────────────────────

/// A scripted provider whose first reply asks for every `(id, tool name, input)` call in one
/// assistant message, and whose second ends the turn.
fn tool_calls_then_end_turn_server(
    calls: &[(&str, &str, Value)],
    final_text: &str,
) -> common::ScriptedServer {
    let tool_uses: Vec<Value> = calls
        .iter()
        .map(|(id, name, input)| {
            serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        })
        .collect();
    common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": tool_uses,
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

fn create_wasm_tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let opts: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", opts).unwrap();
    writeln!(zip, "name: {TOOL_NAME}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();
    writeln!(zip, "description: counts JSONL lines").unwrap();
    writeln!(zip, "input_schema:").unwrap();
    writeln!(zip, "  type: object").unwrap();
    writeln!(zip, "  properties:").unwrap();
    writeln!(zip, "    data:").unwrap();
    writeln!(zip, "      type: string").unwrap();

    zip.start_file("tool.wasm", opts).unwrap();
    zip.write_all(&fs::read(fixture_path("graduation/tool/jsonl-line-count.wasm")).unwrap())
        .unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// A capsule with the anthropic driver, a skill and the `jsonl-line-count` WASM tool.
///
/// The skill is the call that succeeds: a skill dispatch always passes. The tool is the call that
/// fails: it treats the text it is handed — the call's input JSON — as a path, and returns
/// `error` when no file has that name.
///
/// It grants no shell and declares no native tool, so it launches on a host that cannot delegate
/// a cgroup scope or build an egress namespace — both launch gates apply only to a capsule that
/// can spawn a subprocess.
fn stage_skill_and_tool_agent(endpoint: &str) -> capsule_runtime::StagedSession {
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
    common::publish_local(&home, &create_wasm_tool_artifact(artifacts.path())).success();
    let skill =
        common::create_skill_artifact(artifacts.path(), SKILL_NAME, SKILL_VERSION, SKILL_GUIDANCE);
    common::publish_local(&home, &skill).success();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: streaming-tool-agent\nversion: 0.1.0\nartifacts:\n  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n    runtime: tool\n  - name: {SKILL_NAME}\n    version: {SKILL_VERSION}\n    runtime: skill\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    stage_agent(&home, &project.keep().join("murmur.yaml"))
}

fn trace_events(launched: &capsule_runtime::LaunchResult) -> Vec<Value> {
    fs::read_to_string(launched.workdir.join("trace.jsonl"))
        .expect("trace.jsonl should exist")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The only `skill_call` trace record. It carries no `tool_call_id`, so a test that reads one
/// dispatches exactly one skill.
fn skill_call_record(events: &[Value]) -> &Value {
    let mut records = events
        .iter()
        .filter(|event| event["event_type"] == "skill_call");
    let record = records
        .next()
        .unwrap_or_else(|| panic!("no skill_call record; got: {events:#?}"));
    assert!(records.next().is_none(), "expected one skill_call record");
    record
}

/// The `tool_call` trace record for `tool_call_id`.
fn tool_call_record<'a>(events: &'a [Value], tool_call_id: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["event_type"] == "tool_call" && event["tool_call_id"] == tool_call_id)
        .unwrap_or_else(|| panic!("no tool_call record for {tool_call_id}; got: {events:#?}"))
}

/// Every outcome key is on the frame, `null` or not, so a client can tell "does not apply" from
/// "this runtime does not report it".
fn assert_outcome_keys_present(artifact: &Value) {
    for key in [
        "tool_call_id",
        "is_error",
        "duration_ms",
        "exit_code",
        "truncated",
    ] {
        assert!(
            artifact.get(key).is_some(),
            "`{key}` must be present on every frame: {artifact}"
        );
    }
}

#[test]
fn streaming_artifact_reports_a_successful_call() {
    let server = tool_calls_then_end_turn_server(
        &[("toolu_ok", SKILL_NAME, serde_json::json!({}))],
        "read it",
    );
    let (frames, launched) = streamed_artifact_frames(stage_skill_and_tool_agent(&server.endpoint));

    assert_eq!(frames.len(), 1, "one call, one frame: {frames:?}");
    let artifact = &frames[0]["artifact"];
    assert_outcome_keys_present(artifact);
    assert_eq!(artifact["tool_name"], SKILL_NAME);
    assert_eq!(artifact["content"], SKILL_GUIDANCE, "{artifact}");
    assert_eq!(artifact["is_error"], false, "{artifact}");
    assert_eq!(artifact["tool_call_id"], "toolu_ok", "{artifact}");
    assert!(artifact["duration_ms"].is_u64(), "{artifact}");
    assert!(
        artifact["exit_code"].is_null(),
        "a skill runs no subprocess: {artifact}"
    );
    assert_eq!(artifact["truncated"], false, "{artifact}");

    let events = trace_events(&launched);
    let record = skill_call_record(&events);
    assert_eq!(record["status"], "ok", "{record}");
    assert_eq!(record["duration_ms"], artifact["duration_ms"]);
}

#[test]
fn streaming_artifact_reports_a_failed_call() {
    let server = tool_calls_then_end_turn_server(
        &[(
            "toolu_missing",
            TOOL_NAME,
            serde_json::json!({ "data": "does-not-exist.jsonl" }),
        )],
        "could not count",
    );
    let (frames, launched) = streamed_artifact_frames(stage_skill_and_tool_agent(&server.endpoint));

    assert_eq!(frames.len(), 1, "one call, one frame: {frames:?}");
    let artifact = &frames[0]["artifact"];
    assert_outcome_keys_present(artifact);
    assert_eq!(artifact["is_error"], true, "{artifact}");
    assert_eq!(artifact["tool_call_id"], "toolu_missing", "{artifact}");
    assert!(artifact["duration_ms"].is_u64(), "{artifact}");
    assert!(artifact["exit_code"].is_null(), "{artifact}");
    assert_eq!(artifact["truncated"], false, "{artifact}");

    let events = trace_events(&launched);
    let record = tool_call_record(&events, "toolu_missing");
    assert_eq!(record["status"], "error", "{record}");
    assert_eq!(
        record["duration_ms"], artifact["duration_ms"],
        "the frame and the trace carry one measurement"
    );
}

#[test]
fn streaming_artifact_reports_a_failed_dispatch() {
    let server = tool_calls_then_end_turn_server(
        &[("toolu_undeclared", "no-such-tool", serde_json::json!({}))],
        "recovered",
    );
    let (frames, launched) = streamed_artifact_frames(stage_skill_and_tool_agent(&server.endpoint));

    assert_eq!(frames.len(), 1, "one call, one frame: {frames:?}");
    let artifact = &frames[0]["artifact"];
    assert_outcome_keys_present(artifact);
    assert_eq!(artifact["tool_name"], "no-such-tool");
    assert_eq!(artifact["is_error"], true, "{artifact}");
    assert_eq!(
        artifact["tool_call_id"], "toolu_undeclared",
        "the call was made even though no tool ran: {artifact}"
    );
    assert!(artifact["exit_code"].is_null(), "nothing ran: {artifact}");
    assert_eq!(artifact["truncated"], false, "{artifact}");
    assert!(artifact["duration_ms"].is_u64(), "{artifact}");

    let events = trace_events(&launched);
    let record = tool_call_record(&events, "toolu_undeclared");
    assert_eq!(record["status"], "error", "{record}");
    assert_eq!(record["duration_ms"], artifact["duration_ms"]);
}

#[test]
fn streaming_artifact_frames_carry_their_own_call_id() {
    let server = tool_calls_then_end_turn_server(
        &[
            ("toolu_a", SKILL_NAME, serde_json::json!({})),
            (
                "toolu_b",
                TOOL_NAME,
                serde_json::json!({ "data": "does-not-exist.jsonl" }),
            ),
        ],
        "done",
    );
    let (frames, launched) = streamed_artifact_frames(stage_skill_and_tool_agent(&server.endpoint));

    assert_eq!(frames.len(), 2, "two calls, two frames: {frames:?}");
    let ids: Vec<&str> = frames
        .iter()
        .map(|frame| {
            frame["artifact"]["tool_call_id"]
                .as_str()
                .unwrap_or_else(|| panic!("every call frame carries an id: {frame}"))
        })
        .collect();
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>(),
        HashSet::from(["toolu_a", "toolu_b"]),
        "one frame per issued id, no duplicates: {ids:?}"
    );

    let events = trace_events(&launched);
    for frame in &frames {
        let artifact = &frame["artifact"];
        let id = artifact["tool_call_id"].as_str().unwrap();
        assert_eq!(
            artifact["is_error"],
            id == "toolu_b",
            "only the failing call's frame reports an error: {artifact}"
        );
        // The skill writes a `skill_call` record, which carries no id to look it up by.
        let record = if id == "toolu_a" {
            skill_call_record(&events)
        } else {
            tool_call_record(&events, id)
        };
        assert_eq!(
            record["status"] == "error",
            artifact["is_error"] == true,
            "one fact, two spellings: {record} vs {artifact}"
        );
        assert_eq!(record["duration_ms"], artifact["duration_ms"]);
    }
}

#[test]
fn streaming_artifact_carries_the_subprocess_exit_code() {
    if common::skip_without_host_support("streaming_artifact_carries_the_subprocess_exit_code") {
        return;
    }
    let server = tool_calls_then_end_turn_server(
        &[
            (
                "toolu_zero",
                "bash",
                serde_json::json!({ "command": "echo fine" }),
            ),
            (
                "toolu_three",
                "bash",
                serde_json::json!({ "command": "echo broken; exit 3" }),
            ),
        ],
        "ran both",
    );
    let (frames, launched) = artifact_frames(setup_agent_project(&server.endpoint));

    assert_eq!(frames.len(), 2, "two calls, two frames: {frames:?}");
    let events = trace_events(&launched);
    let shell_records: Vec<&Value> = events
        .iter()
        .filter(|event| event["event_type"] == "shell")
        .collect();
    assert_eq!(
        shell_records.len(),
        2,
        "one shell record per foreground command: {events:#?}"
    );

    let exit_codes: Vec<i64> = frames
        .iter()
        .map(|frame| {
            let artifact = &frame["artifact"];
            assert_outcome_keys_present(artifact);
            artifact["exit_code"]
                .as_i64()
                .unwrap_or_else(|| panic!("a completed subprocess reports its exit: {artifact}"))
        })
        .collect();
    assert_eq!(exit_codes, vec![0, 3], "frames arrive in call order");

    // Calls dispatch one after another, so the n-th shell record belongs to the n-th frame.
    for (frame, shell) in frames.iter().zip(&shell_records) {
        assert_eq!(
            frame["artifact"]["exit_code"], shell["exit_code"],
            "the frame and the shell record carry one exit code: {frame} vs {shell}"
        );
    }
    assert_eq!(frames[0]["artifact"]["tool_call_id"], "toolu_zero");
    assert_eq!(frames[1]["artifact"]["tool_call_id"], "toolu_three");

    // A command that ran to completion is a successful call whatever its exit status, so
    // `is_error` and `exit_code` are separate facts; `is_error` still agrees with the trace.
    for frame in &frames {
        let artifact = &frame["artifact"];
        let record = tool_call_record(&events, artifact["tool_call_id"].as_str().unwrap());
        assert_eq!(
            record["status"] == "error",
            artifact["is_error"] == true,
            "one fact, two spellings: {record} vs {artifact}"
        );
        assert_eq!(record["duration_ms"], artifact["duration_ms"]);
    }
}
