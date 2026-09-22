//! A launched, idle `queue` + `sleep` agent capsule and the raw-socket helpers that drive it:
//! submitting tasks, waiting on the trace, and reading a `stream/watch` connection byte for byte.
//!
//! The readers keep SSE comment lines, which `mur watch` and the parsed SSE clients elsewhere in
//! the suite drop, so a test can observe the heartbeat.

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

use super::ScriptedServer;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

// ── provider replies ───────────────────────────────────────────────────────────

/// An anthropic `end_turn` reply numbered `n`, saying `text`.
pub fn end_turn(n: usize, text: &str) -> String {
    serde_json::json!({
        "id": format!("msg_{n}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One `end_turn` reply per entry of `texts`, in order.
pub fn end_turns(texts: &[&str]) -> Vec<String> {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| end_turn(i + 1, text))
        .collect()
}

pub fn end_turn_server(texts: &[&str]) -> ScriptedServer {
    ScriptedServer::start(end_turns(texts))
}

// ── capsule ────────────────────────────────────────────────────────────────────

/// A launched, idle queue+sleep capsule: its address and its workdir.
pub struct IdleCapsule {
    pub url: String,
    pub workdir: PathBuf,
    _home: TempDir,
    _server: ScriptedServer,
}

/// Launch a capsule declaring only the anthropic driver against `server`.
pub fn launch_idle_capsule(server: ScriptedServer) -> IdleCapsule {
    launch_idle_capsule_with(server, |_, _| String::new())
}

/// [`launch_idle_capsule`], with `extra_artifacts` publishing further artifacts into the home it
/// is handed (writing any files under the directory it is handed) and returning their
/// `artifacts:` entries as YAML.
pub fn launch_idle_capsule_with(
    server: ScriptedServer,
    extra_artifacts: impl FnOnce(&TempDir, &Path) -> String,
) -> IdleCapsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = super::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &super::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    super::publish_local(&home, &driver_artifact).success();
    let extra = extra_artifacts(&home, artifacts.path());

    let endpoint = &server.endpoint;
    let manifest_path = project.keep().join("murmur.yaml");
    fs::write(
        &manifest_path,
        format!(
            "name: idle-queue-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n{extra}capabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

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

// ── raw SSE reading ────────────────────────────────────────────────────────────

/// Open a `stream/watch` connection and return the socket, positioned at the first byte
/// of the HTTP response.
pub fn open_watch(addr: &str, last_event_id: u64) -> TcpStream {
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

/// Splits a connection's bytes into lines, keeping an incomplete line for the next read.
pub struct LineReader<'a> {
    stream: &'a TcpStream,
    pending: Vec<u8>,
}

impl<'a> LineReader<'a> {
    pub fn new(stream: &'a TcpStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
        }
    }

    /// Read until `deadline` or end of stream, returning every line completed in that time with
    /// the moment it arrived.
    pub fn read_until(&mut self, deadline: Instant) -> Vec<(Instant, String)> {
        let mut source: &TcpStream = self.stream;
        let mut lines = Vec::new();
        let mut buf = [0u8; 4096];

        while Instant::now() < deadline {
            match source.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let now = Instant::now();
                    self.pending.extend_from_slice(&buf[..n]);
                    while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
                        let raw: Vec<u8> = self.pending.drain(..=pos).collect();
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
}

/// Read raw bytes until `deadline`, returning every complete line with the moment it arrived.
pub fn read_lines_until(stream: &TcpStream, deadline: Instant) -> Vec<(Instant, String)> {
    LineReader::new(stream).read_until(deadline)
}

/// Drop the HTTP status line and headers, returning only the SSE body lines.
pub fn sse_body(lines: &[(Instant, String)]) -> Vec<(Instant, String)> {
    let mut iter = lines.iter();
    for (_, line) in iter.by_ref() {
        if line.is_empty() {
            break;
        }
    }
    iter.cloned().collect()
}

// ── task submission ────────────────────────────────────────────────────────────

pub fn http_post_json(addr: &str, body: &str) -> Value {
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

/// Submit a task with `message/send`, assert it was accepted, and return its id.
pub fn submit_task(addr: &str, message_id: &str, text: &str) -> String {
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
    response["result"]["id"].as_str().unwrap().to_string()
}

/// Wait until the session trace holds `expected` `task_end` records.
pub fn wait_for_task_ends(workdir: &Path, expected: usize) {
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
