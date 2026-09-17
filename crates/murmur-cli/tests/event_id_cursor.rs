//! Integration tests for the session-wide event-id sequence against a real launched capsule.
//!
//! Every numbered frame a capsule session writes takes the next id of one sequence, whatever
//! task it belongs to, so `Last-Event-ID` resumes exactly after the frame it names.

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
            "name: event-id-cursor-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
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

// ── frames ─────────────────────────────────────────────────────────────────────

/// One SSE frame as written: its `id:` (if any), its event type and its raw data line.
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    id: Option<u64>,
    event: String,
    data: String,
}

impl Frame {
    /// The task id in the frame's `data`, for the frames that carry one.
    fn task_id(&self) -> Option<String> {
        serde_json::from_str::<Value>(&self.data)
            .ok()?
            .get("id")?
            .as_str()
            .map(str::to_string)
    }
}

/// Parse SSE body lines into frames, dropping comments such as the heartbeat.
fn frames(body: &[(Instant, String)]) -> Vec<Frame> {
    let mut out = Vec::new();
    let (mut id, mut event, mut data) = (None, String::new(), String::new());
    for (_, line) in body {
        if line.is_empty() {
            if !event.is_empty() {
                out.push(Frame {
                    id,
                    event: std::mem::take(&mut event),
                    data: std::mem::take(&mut data),
                });
            }
            id = None;
        } else if let Some(rest) = line.strip_prefix("id: ") {
            id = rest.parse().ok();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            event = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data = rest.to_string();
        }
    }
    out
}

/// Launch a capsule, run two tasks to completion and return it with the full replay a watcher
/// attached with `Last-Event-ID: 0` receives.
fn two_tasks_replayed() -> (IdleCapsule, Vec<Frame>) {
    let capsule = launch_idle_capsule(&["first reply", "second reply"]);
    submit_task(&capsule.url, "event-id-cursor-1", "first task");
    wait_for_task_ends(&capsule.workdir, 1);
    submit_task(&capsule.url, "event-id-cursor-2", "second task");
    wait_for_task_ends(&capsule.workdir, 2);

    let replay = watch_frames(&capsule.url, 0);
    (capsule, replay)
}

/// The numbered frames a `stream/watch` attached with `last_event_id` receives within 3 s,
/// after asserting nothing but `connection-ack` arrives without an id.
fn watch_frames(addr: &str, last_event_id: u64) -> Vec<Frame> {
    let conn = open_watch(addr, last_event_id);
    let body = sse_body(&read_lines_until(
        &conn,
        Instant::now() + Duration::from_secs(3),
    ));
    let all = frames(&body);
    assert_eq!(
        all.first().map(|f| f.event.as_str()),
        Some("connection-ack"),
        "stream/watch should open with connection-ack; frames were {all:#?}"
    );
    all.into_iter().skip(1).collect()
}

/// The distinct task ids in the order their first frame was written.
fn task_order(frames: &[Frame]) -> Vec<String> {
    let mut order = Vec::new();
    for task in frames.iter().filter_map(Frame::task_id) {
        if !order.contains(&task) {
            order.push(task);
        }
    }
    order
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// A replay from `0` of a session that has evicted nothing is every frame, numbered `1..=n` with
/// no repeat and no jump, and the second task's frames continue the first task's sequence.
#[test]
fn ids_ascend_across_tasks() {
    if common::skip_without_host_support("ids_ascend_across_tasks") {
        return;
    }
    let (_capsule, replay) = two_tasks_replayed();

    assert!(
        replay.iter().all(|f| f.event != "gap"),
        "a replay from 0 of an unevicted buffer should carry no gap; frames were {replay:#?}"
    );
    let ids: Vec<u64> = replay
        .iter()
        .map(|f| {
            f.id.unwrap_or_else(|| panic!("replayed frame without an id: {f:?}"))
        })
        .collect();
    let expected: Vec<u64> = (1..=ids.len() as u64).collect();
    assert_eq!(ids, expected, "ids should be exactly 1..=n");

    let first = &replay[0];
    assert_eq!(first.event, "status", "id 1 should be a status: {first:?}");
    assert!(
        first.data.contains(r#""state":"working""#),
        "id 1 should be the first task's working status: {first:?}"
    );

    let tasks = task_order(&replay);
    assert_eq!(
        tasks.len(),
        2,
        "expected two tasks; frames were {replay:#?}"
    );
    let ids_of = |task: &str| -> Vec<u64> {
        replay
            .iter()
            .filter(|f| f.task_id().as_deref() == Some(task))
            .filter_map(|f| f.id)
            .collect()
    };
    let (first_ids, second_ids) = (ids_of(&tasks[0]), ids_of(&tasks[1]));
    assert!(
        !second_ids.is_empty(),
        "the second task should have frames; frames were {replay:#?}"
    );
    assert!(
        second_ids.iter().min() > first_ids.iter().max(),
        "every second-task id should follow every first-task id; first {first_ids:?}, second {second_ids:?}"
    );
}

/// Reconnecting with the id of the first task's final status replays exactly the frames written
/// after it — every frame of the second task — with no gap.
#[test]
fn resume_from_first_task_cursor() {
    if common::skip_without_host_support("resume_from_first_task_cursor") {
        return;
    }
    let (capsule, replay) = two_tasks_replayed();

    let tasks = task_order(&replay);
    assert_eq!(
        tasks.len(),
        2,
        "expected two tasks; frames were {replay:#?}"
    );
    let cursor = replay
        .iter()
        .find(|f| {
            f.event == "status"
                && f.task_id().as_deref() == Some(tasks[0].as_str())
                && f.data.contains(r#""final":true"#)
        })
        .and_then(|f| f.id)
        .unwrap_or_else(|| panic!("no final status for the first task; frames were {replay:#?}"));

    let resumed = watch_frames(&capsule.url, cursor);

    assert!(
        resumed.iter().all(|f| f.event != "gap"),
        "resuming from a buffered id should carry no gap; frames were {resumed:#?}"
    );
    let expected: Vec<Frame> = replay
        .iter()
        .filter(|f| f.id.is_some_and(|id| id > cursor))
        .cloned()
        .collect();
    assert_eq!(
        resumed, expected,
        "the resumed replay should be exactly the frames after id {cursor}"
    );
    let second_task_frames = replay
        .iter()
        .filter(|f| f.task_id().as_deref() == Some(tasks[1].as_str()))
        .count();
    assert_eq!(
        resumed
            .iter()
            .filter(|f| f.task_id().as_deref() == Some(tasks[1].as_str()))
            .count(),
        second_task_frames,
        "the resumed replay should include every frame of the second task"
    );
    assert!(
        resumed
            .iter()
            .all(|f| f.task_id().as_deref() != Some(tasks[0].as_str())),
        "the resumed replay should include no frame of the first task; frames were {resumed:#?}"
    );
}
