//! Integration tests for the `stream/watch` liveness signal against a real launched capsule.
//!
//! These read raw bytes rather than parsed events: the heartbeat is an SSE comment, and the
//! SSE clients elsewhere in the tree (and `mur watch` itself) discard comment lines by design,
//! so they cannot see what these tests measure.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The interval `handle_stream_watch` writes `:heartbeat` at. Mirrored from
/// `capsule_runtime::streaming::SSE_HEARTBEAT_INTERVAL`, which is crate-private.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

// ── capsule staging ────────────────────────────────────────────────────────────

fn end_turn_server(texts: &[&str]) -> common::ScriptedServer {
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
        format!(
            "name: watch-heartbeat-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
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
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
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
            // queue + sleep: the capsule never exits on its own, so it can be left idle.
            lifecycle: Some(LifecycleConfig {
                task_acceptance: TaskAcceptance::Queue,
                after_task: AfterTask::Sleep,
                queue_depth: 2,
                input_timeout_secs: None,
                ..Default::default()
            }),
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

/// A launched, idle queue+sleep capsule: its address and its workdir.
struct IdleCapsule {
    url: String,
    workdir: PathBuf,
    _home: TempDir,
    _server: common::ScriptedServer,
}

fn launch_idle_capsule(replies: &[&str]) -> IdleCapsule {
    let server = end_turn_server(replies);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);
    let workdir = staged.workdir.clone();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    // The capsule never exits in queue+sleep, so the join handle is deliberately dropped.
    std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("timed out waiting for capsule_url");

    IdleCapsule {
        url,
        workdir,
        _home: home,
        _server: server,
    }
}

// ── raw SSE reading ────────────────────────────────────────────────────────────

/// Open a `stream/watch` connection and return the socket, positioned at the first byte
/// of the SSE body.
fn open_watch(addr: &str, last_event_id: u64) -> TcpStream {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"stream/watch","params":{}}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nLast-Event-ID: {last_event_id}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );

    let stream = TcpStream::connect(addr).expect("should connect to capsule");
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    (&stream).write_all(request.as_bytes()).unwrap();
    (&stream).flush().unwrap();
    stream
}

/// Read raw bytes until `deadline`, returning every complete line with the moment it arrived.
///
/// Keeps comment lines, which is the whole reason this exists: `collect_sse_events` in
/// `streaming.rs` and `mur watch` both drop them, so neither can observe a heartbeat.
fn read_lines_until(stream: &TcpStream, deadline: Instant) -> Vec<(Instant, String)> {
    let mut source: &TcpStream = stream;
    let mut lines = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    while Instant::now() < deadline {
        match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let now = Instant::now();
                pending.extend_from_slice(&buf[..n]);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let raw: Vec<u8> = pending.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&raw[..raw.len() - 1])
                        .trim_end_matches('\r')
                        .to_string();
                    lines.push((now, text));
                }
            }
            // A read timeout is the normal case on an idle stream.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    lines
}

/// Drop the HTTP status line and headers, returning only the SSE body lines.
fn sse_body(lines: &[(Instant, String)]) -> Vec<(Instant, String)> {
    let mut iter = lines.iter();
    for (_, line) in iter.by_ref() {
        if line.is_empty() {
            break;
        }
    }
    iter.cloned().collect()
}

fn heartbeat_arrivals(body: &[(Instant, String)], since: Instant) -> Vec<Duration> {
    body.iter()
        .filter(|(_, line)| line == ":heartbeat")
        .map(|(at, _)| at.duration_since(since))
        .collect()
}

// ── task submission ────────────────────────────────────────────────────────────

fn http_post_json(addr: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    reader.read_to_string(&mut response_body).ok();
    serde_json::from_str(&response_body)
        .unwrap_or_else(|_| serde_json::json!({"_raw": response_body}))
}

fn submit_task(addr: &str, message_id: &str, text: &str) {
    let body = serde_json::json!({
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
    .to_string();
    let response = http_post_json(addr, &body);
    assert_eq!(
        response["result"]["status"]["state"], "submitted",
        "task should be submitted; got: {response}"
    );
}

fn wait_for_task_ends(workdir: &Path, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let trace = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
        if trace.lines().filter(|l| l.contains("\"task_end\"")).count() >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} task(s) to finish; trace:\n{trace}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// An observer on an idle capsule can tell the connection is alive by reading bytes.
///
/// One heartbeat is the expected count in a 25 s window: the interval is 15 s and the first
/// tick is consumed immediately. Two is scheduling slack; three would mean a burst of missed
/// ticks.
#[test]
fn watch_idle_observer_receives_heartbeat() {
    let capsule = launch_idle_capsule(&["idle capsule reply"]);

    let started = Instant::now();
    let stream = open_watch(&capsule.url, 0);
    let lines = read_lines_until(&stream, started + Duration::from_secs(25));
    let body = sse_body(&lines);

    assert!(
        body.iter().any(|(_, line)| line == "event: connection-ack"),
        "stream/watch should open with a connection-ack; body was: {:?}",
        body.iter().map(|(_, l)| l).collect::<Vec<_>>()
    );

    let arrivals = heartbeat_arrivals(&body, started);
    assert!(
        arrivals.iter().any(|d| *d <= Duration::from_secs(20)),
        "expected a :heartbeat within 20 s of connecting; arrivals were {arrivals:?}"
    );
    assert!(
        arrivals.len() <= 2,
        "expected at most 2 heartbeats in a 25 s window at a {HEARTBEAT_INTERVAL:?} interval, \
         got {}: {arrivals:?}",
        arrivals.len()
    );
}

/// The heartbeat is a comment, not a frame: it consumes no event id and never enters the
/// replay buffer.
///
/// Three replays of the same capsule state from the same `Last-Event-ID` — one too short to
/// see a heartbeat, one held across one, one opened afterwards — must return the identical
/// `id:` sequence and the identical body. The third is what catches a heartbeat that reached
/// the replay buffer; the second is what catches one that took an id or arrived as a frame.
#[test]
fn watch_heartbeat_does_not_perturb_event_ids() {
    let capsule = launch_idle_capsule(&["replayable reply"]);

    // Put real events in the replay buffer before observing.
    submit_task(&capsule.url, "heartbeat-ids-1", "produce some events");
    wait_for_task_ends(&capsule.workdir, 1);

    // Before: too short a connection to have seen a heartbeat.
    let before_conn = open_watch(&capsule.url, 0);
    let before_body = sse_body(&read_lines_until(
        &before_conn,
        Instant::now() + Duration::from_secs(3),
    ));
    drop(before_conn);

    let before_ids = event_ids(&before_body);
    assert!(
        !before_ids.is_empty(),
        "expected the replay to carry event ids; body was: {:?}",
        text(&before_body)
    );
    assert_eq!(
        heartbeat_count(&before_body),
        0,
        "a 3 s window should be far too short for a {HEARTBEAT_INTERVAL:?} heartbeat"
    );

    // Across: a connection held past one heartbeat interval.
    let started = Instant::now();
    let across_conn = open_watch(&capsule.url, 0);
    let across_body = sse_body(&read_lines_until(
        &across_conn,
        started + HEARTBEAT_INTERVAL + Duration::from_secs(5),
    ));
    drop(across_conn);

    assert!(
        heartbeat_count(&across_body) > 0,
        "expected at least one heartbeat while holding the connection open; body was: {:?}",
        text(&across_body)
    );
    // Every heartbeat stands alone between blank lines — no id:, no event: of its own.
    for block in blocks(&across_body) {
        if block.iter().any(|line| line.starts_with(':')) {
            assert_eq!(
                block,
                vec![":heartbeat".to_string()],
                "a heartbeat shared a frame with other lines: {block:?}"
            );
        }
    }
    assert_eq!(
        event_ids(&across_body),
        before_ids,
        "a heartbeat added or consumed an event id on the connection carrying it"
    );

    // After: the replay a later observer gets must be untouched by the heartbeat.
    let after_conn = open_watch(&capsule.url, 0);
    let after_body = sse_body(&read_lines_until(
        &after_conn,
        Instant::now() + Duration::from_secs(3),
    ));
    drop(after_conn);

    assert_eq!(
        heartbeat_count(&after_body),
        0,
        "a heartbeat was replayed, so it entered the replay buffer; body was: {:?}",
        text(&after_body)
    );
    assert_eq!(
        event_ids(&after_body),
        before_ids,
        "the id sequence replayed from Last-Event-ID: 0 changed once a heartbeat had been written"
    );
    assert_eq!(
        text(&after_body),
        text(&before_body),
        "the replayed body changed once a heartbeat had been written"
    );
}

fn text(body: &[(Instant, String)]) -> Vec<String> {
    body.iter().map(|(_, line)| line.clone()).collect()
}

fn heartbeat_count(body: &[(Instant, String)]) -> usize {
    body.iter().filter(|(_, line)| line == ":heartbeat").count()
}

/// Split SSE body lines into blank-line-delimited frames.
fn blocks(body: &[(Instant, String)]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for (_, line) in body {
        if line.is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line.clone());
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn event_ids(body: &[(Instant, String)]) -> Vec<u64> {
    body.iter()
        .filter_map(|(_, line)| line.strip_prefix("id: "))
        .filter_map(|rest| rest.trim().parse::<u64>().ok())
        .collect()
}
