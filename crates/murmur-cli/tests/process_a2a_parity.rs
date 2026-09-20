//! What a client watching a `transport: process` capsule sees, held against what it sees watching
//! an `http` capsule doing the same work.
//!
//! Each capsule is its own `mur run --json` process, so a scenario's harness profile and debug
//! overrides live in that child's environment and nowhere else. The process side always drives the
//! fixture fake harness — never a real harness CLI — which is a bash script, so these tests are
//! Unix-only.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const HTTP_DRIVER: &str = "murmur-driver-anthropic";
const HTTP_DRIVER_VERSION: &str = "0.1.4";
const STREAMING_DRIVER: &str = "streaming-driver";
const PROCESS_DRIVER: &str = "fixture-process-driver";
const VERSION: &str = "0.1.0";
const TOOL: &str = "echo-tool";

/// How long a stream may go without a frame before the collector gives up. A run that leaves the
/// connection open is a failure, so this is the ceiling no scenario may reach.
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

// ── Artifacts ─────────────────────────────────────────────────────────────────

fn create_tool_artifact(dir: &Path, wasm: &Path) -> PathBuf {
    let path = dir.join(format!("{TOOL}-{VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {TOOL}").unwrap();
    writeln!(zip, "version: {VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(wasm).unwrap()).unwrap();
    zip.finish().unwrap();
    path
}

fn create_streaming_driver_artifact(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{STREAMING_DRIVER}-{VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {STREAMING_DRIVER}").unwrap();
    writeln!(zip, "version: {VERSION}").unwrap();
    writeln!(zip, "runtime: driver").unwrap();
    writeln!(zip, "upstream_auth:").unwrap();
    writeln!(zip, "  header: x-api-key").unwrap();
    writeln!(zip, "  value: \"{{key}}\"").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(
        &fs::read(common::fixture_path(
            "streaming-driver/tool/streaming-driver.wasm",
        ))
        .unwrap(),
    )
    .unwrap();
    zip.finish().unwrap();
    path
}

fn publish(home: &TempDir, artifact: &Path) {
    common::publish_local(home, artifact).success();
}

// ── A capsule running as its own process ──────────────────────────────────────

struct Capsule {
    child: Child,
    _home: TempDir,
    _project: TempDir,
    startup: Value,
}

impl Capsule {
    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start `mur run` on `manifest` and block until it has printed its address.
fn start(home: TempDir, project: TempDir, manifest: &Path, env: &[(&str, &str)]) -> Capsule {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .args(["run", "--manifest"])
        .arg(manifest)
        .arg("--json")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("mur run should start");

    let (startup_tx, startup_rx) = mpsc::channel::<Value>();
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                if value.get("url").is_some() {
                    let _ = startup_tx.send(value);
                }
            }
        }
    });
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[capsule] {line}");
        }
    });

    match startup_rx.recv_timeout(Duration::from_secs(180)) {
        Ok(startup) => Capsule {
            child,
            _home: home,
            _project: project,
            startup,
        },
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for the capsule to print its address");
        }
    }
}

/// A capsule that serves its door and stays up after a task, so a test can read its card after
/// the stream has closed.
const LIFECYCLE: &str =
    "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n";

/// An `http` capsule driven by `server`, optionally declaring the echo tool.
fn http_capsule(server: &common::ScriptedServer, name: &str, tool: bool) -> Capsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    publish(
        &home,
        &common::create_driver_artifact(
            artifacts.path(),
            HTTP_DRIVER,
            HTTP_DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        ),
    );
    let mut entries = format!(
        "  - name: {HTTP_DRIVER}\n    version: {HTTP_DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {}\n      api_key: test-key\n",
        server.endpoint
    );
    if tool {
        publish(
            &home,
            &create_tool_artifact(
                artifacts.path(),
                &common::fixture_path("run/components/echo-tool.wasm"),
            ),
        );
        entries.push_str(&format!(
            "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n"
        ));
    }

    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: {name}\nversion: 0.1.0\nartifacts:\n{entries}\
             capabilities:\n  network:\n    allow:\n      - {}\n{LIFECYCLE}\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {HTTP_DRIVER}\n",
            server.endpoint
        ),
    )
    .unwrap();
    start(home, project, &manifest, &[])
}

/// An `http` capsule whose driver streams its text in three chunks and needs no provider.
fn streaming_http_capsule(name: &str) -> Capsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    publish(&home, &create_streaming_driver_artifact(artifacts.path()));
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {STREAMING_DRIVER}\n    version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:1\n      api_key: test-key\n\
             {LIFECYCLE}inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {STREAMING_DRIVER}\n"
        ),
    )
    .unwrap();
    start(home, project, &manifest, &[])
}

/// How a process capsule under test differs from the plain one.
struct ProcessCapsule {
    profile: String,
    name: String,
    tool: bool,
    /// `inference.driver.config`, as manifest lines under `driver:`.
    config: Option<String>,
    /// Extra environment for the `mur` process, for the debug inactivity override.
    env: Vec<(String, String)>,
}

impl ProcessCapsule {
    fn new(name: &str, profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            name: name.to_string(),
            tool: false,
            config: None,
            env: Vec::new(),
        }
    }

    fn with_tool(mut self) -> Self {
        self.tool = true;
        self
    }

    fn config(mut self, config: &str) -> Self {
        self.config = Some(config.to_string());
        self
    }

    fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    fn start(self) -> Capsule {
        let home = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        publish(
            &home,
            &common::create_driver_artifact_with_auth(
                artifacts.path(),
                PROCESS_DRIVER,
                VERSION,
                &common::fixture_path("process-driver/tool/process-driver.wasm"),
                "",
            ),
        );
        let mut entries =
            format!("  - name: {PROCESS_DRIVER}\n    version: {VERSION}\n    runtime: driver\n");
        if self.tool {
            publish(
                &home,
                &create_tool_artifact(
                    artifacts.path(),
                    &common::fixture_path("run/components/echo-tool.wasm"),
                ),
            );
            entries.push_str(&format!(
                "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n"
            ));
        }

        // The fake harness, copied where this capsule alone points at it.
        let harness = project.path().join("fake-harness");
        fs::copy(
            common::fixture_path("process-driver/fake-harness"),
            &harness,
        )
        .unwrap();
        let mut perms = fs::metadata(&harness).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&harness, perms).unwrap();

        let name = &self.name;
        let config = self.config.as_deref().unwrap_or_default();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: {name}\nversion: 0.1.0\nartifacts:\n{entries}\
                 capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
                 {LIFECYCLE}\
                 inference:\n  transport: process\n  driver:\n    artifact: {PROCESS_DRIVER}\n{config}\
                 \x20 command: {}\n",
                harness.to_str().unwrap()
            ),
        )
        .unwrap();

        let mut env: Vec<(&str, &str)> = vec![
            ("FIXTURE_HARNESS_PROFILE", self.profile.as_str()),
            ("PATH", path_value()),
        ];
        for (key, value) in &self.env {
            env.push((key.as_str(), value.as_str()));
        }
        start(home, project, &manifest, &env)
    }
}

/// The host's `PATH`, which the fake harness needs for `sleep` and its own interpreter.
fn path_value() -> &'static str {
    Box::leak(
        std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/bin:/bin".to_string())
            .into_boxed_str(),
    )
}

// ── The provider the http parity capsule answers to ───────────────────────────

fn tool_then_answer_server() -> common::ScriptedServer {
    common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "call_1",
                "name": TOOL,
                "input": { "msg": "ping" }
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
            "content": [{"type": "text", "text": "PARITY-ANSWER"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ])
}

// ── SSE client ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SseEvent {
    event_type: String,
    data: String,
}

impl SseEvent {
    fn json(&self) -> Value {
        serde_json::from_str(&self.data).expect("every frame's data is JSON")
    }
}

/// Open a `message/stream` connection and read until the first `status` frame with
/// `"final":true`. A `text` frame with `"final":true` is not terminal.
fn collect_sse_events(addr: &str, timeout: Duration) -> Vec<SseEvent> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {
            "message": {
                "messageId": "parity-1",
                "role": "user",
                "parts": [{"text": "do the parity thing"}]
            }
        }
    })
    .to_string();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );

    let stream = TcpStream::connect(addr).expect("should connect to the capsule");
    stream.set_read_timeout(Some(timeout)).ok();
    {
        let mut writer = &stream;
        writer.write_all(request.as_bytes()).unwrap();
        let _ = writer.flush();
    }

    let mut reader = BufReader::new(&stream);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).is_err() {
        return vec![];
    }
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
    let mut event_type = String::new();
    let mut data = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string();
        if line.is_empty() {
            if !event_type.is_empty() && !data.is_empty() {
                let terminal = event_type == "status" && data.contains("\"final\":true");
                events.push(SseEvent {
                    event_type: event_type.clone(),
                    data: data.clone(),
                });
                if terminal {
                    break;
                }
            }
            event_type.clear();
            data.clear();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data = rest.to_string();
        }
    }
    events
}

fn http_get(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("should connect to the card endpoint");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut reader = BufReader::new(&stream);
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

// ── Frame labelling ───────────────────────────────────────────────────────────

/// What each frame is, in one word per frame: the sequences two transports are compared by.
fn frame_kinds(events: &[SseEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            let data = event.json();
            match event.event_type.as_str() {
                "status" => match data["status"]["state"].as_str().unwrap_or_default() {
                    "working" => format!(
                        "status:working:{}",
                        data["status"]["message"].as_str().unwrap_or_default()
                    ),
                    state => format!("status:{state}"),
                },
                "text" => match data["final"] == Value::Bool(true) {
                    true => "text:final".to_string(),
                    false => "text:chunk".to_string(),
                },
                other => other.to_string(),
            }
        })
        .collect()
}

/// Assert two streams carry the same frames, printing both sequences when they do not.
fn assert_same_frames(http: &[SseEvent], process: &[SseEvent], expected: &[&str]) {
    let http_kinds = frame_kinds(http);
    let process_kinds = frame_kinds(process);
    println!("http:    {http_kinds:?}");
    println!("process: {process_kinds:?}");
    assert_eq!(
        http_kinds, process_kinds,
        "the two transports wrote different frames\n  http:    {http_kinds:?}\n  process: {process_kinds:?}"
    );
    assert_eq!(
        http_kinds,
        expected.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        "the frames are not the ones this card fixes\n  got: {http_kinds:?}"
    );
}

fn frames_of(events: &[SseEvent], event_type: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.event_type == event_type)
        .map(SseEvent::json)
        .collect()
}

fn final_status(events: &[SseEvent]) -> Value {
    let finals: Vec<Value> = frames_of(events, "status")
        .into_iter()
        .filter(|frame| frame["final"] == Value::Bool(true))
        .collect();
    assert_eq!(
        finals.len(),
        1,
        "an attempt writes exactly one final status frame; got {finals:#?}"
    );
    finals.into_iter().next().unwrap()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// S1. The same work — text, one tool call, an answer — writes the same frames on both
/// transports.
#[test]
fn text_a_tool_call_and_an_answer_write_the_same_frames() {
    if common::skip_without_host_support("text_a_tool_call_and_an_answer_write_the_same_frames") {
        return;
    }
    let server = tool_then_answer_server();
    let http = http_capsule(&server, "parity-http", true);
    let http_events = collect_sse_events(&http.url(), STREAM_TIMEOUT);

    let process = ProcessCapsule::new("parity-process", "parity")
        .with_tool()
        .start();
    let process_events = collect_sse_events(&process.url(), STREAM_TIMEOUT);

    assert_same_frames(
        &http_events,
        &process_events,
        &[
            "status:working:inference turn 1",
            "artifact",
            "status:working:inference turn 2",
            "text:final",
            "status:completed",
        ],
    );

    for (transport, events) in [("http", &http_events), ("process", &process_events)] {
        let artifact = &frames_of(events, "artifact")[0]["artifact"];
        assert_eq!(artifact["tool_name"], TOOL, "{transport}: {artifact}");
        assert_eq!(artifact["is_error"], false, "{transport}: {artifact}");
        assert!(
            artifact["tool_call_id"].is_string(),
            "{transport}: {artifact}"
        );
        assert!(
            artifact["duration_ms"].is_number(),
            "{transport}: {artifact}"
        );
        assert_eq!(
            final_status(events)["status"]["response"],
            "PARITY-ANSWER",
            "{transport}"
        );
    }
}

/// S2. Streamed text arrives in the same shape on both transports: one chunk per fragment, then
/// the cursor removal.
#[test]
fn streamed_text_arrives_in_the_same_shape() {
    if common::skip_without_host_support("streamed_text_arrives_in_the_same_shape") {
        return;
    }
    let http = streaming_http_capsule("stream-http");
    let http_events = collect_sse_events(&http.url(), STREAM_TIMEOUT);

    let process = ProcessCapsule::new("stream-process", "stream").start();
    let process_events = collect_sse_events(&process.url(), STREAM_TIMEOUT);

    assert_same_frames(
        &http_events,
        &process_events,
        &[
            "status:working:inference turn 1",
            "text:chunk",
            "text:chunk",
            "text:chunk",
            "text:final",
            "status:completed",
        ],
    );

    for (transport, events) in [("http", &http_events), ("process", &process_events)] {
        let texts = frames_of(events, "text");
        let chunks: Vec<&str> = texts[..3]
            .iter()
            .map(|frame| frame["text"].as_str().unwrap())
            .collect();
        for (frame, chunk) in texts[..3].iter().zip(&chunks) {
            assert_eq!(frame["final"], false, "{transport}: {frame}");
            assert!(!chunk.is_empty(), "{transport}: {frame}");
        }
        assert_eq!(
            chunks.concat(),
            "chunk_one chunk_two chunk_three",
            "{transport}"
        );
        assert_eq!(texts[3]["text"], "", "{transport}: the cursor removal");
        assert_eq!(texts[3]["final"], true, "{transport}");
    }
}

/// S3. A harness that fails its turn on auth reaches the client as a `failed` status naming it,
/// and the connection closes on that frame.
#[test]
fn a_failed_harness_turn_reaches_the_client() {
    if common::skip_without_host_support("a_failed_harness_turn_reaches_the_client") {
        return;
    }
    let capsule = ProcessCapsule::new("fail-auth-process", "fail-auth").start();
    let started = Instant::now();
    let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    assert!(
        started.elapsed() < STREAM_TIMEOUT,
        "the stream was still open when the collector gave up"
    );

    println!("process: {:?}", frame_kinds(&events));
    let last = events.last().expect("the stream carried frames");
    assert_eq!(last.event_type, "status");
    let status = final_status(&events);
    assert_eq!(status["status"]["state"], "failed", "{status}");
    let message = status["status"]["message"].as_str().unwrap();
    assert!(message.contains("auth"), "{message}");
    assert!(message.contains("the harness says so"), "{message}");
}

/// S4. Every way an attempt can end writes exactly one terminal status, and none of them leaves
/// the connection open.
#[test]
fn every_way_an_attempt_ends_writes_one_terminal_status() {
    if common::skip_without_host_support("every_way_an_attempt_ends_writes_one_terminal_status") {
        return;
    }
    let cases: Vec<(&str, ProcessCapsule, &str, Option<&str>)> = vec![
        (
            "turn-end",
            ProcessCapsule::new("ends-completed", "parity").with_tool(),
            "completed",
            None,
        ),
        (
            "turn-failed",
            ProcessCapsule::new("ends-failed", "fail-auth"),
            "failed",
            None,
        ),
        (
            "inactivity",
            ProcessCapsule::new("ends-inactive", "silent")
                .env("MURMUR_DEBUG_PROCESS_INACTIVITY_MS", "1500"),
            "failed",
            Some("E-RUN-035"),
        ),
        (
            "refused launch",
            ProcessCapsule::new("ends-refused", "parity")
                .config("    config:\n      refuse-launch: true\n"),
            "failed",
            Some("E-RUN-034"),
        ),
    ];

    for (what, capsule, state, code) in cases {
        let capsule = capsule.start();
        let started = Instant::now();
        let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
        println!("{what}: {:?}", frame_kinds(&events));
        assert!(
            started.elapsed() < STREAM_TIMEOUT,
            "{what}: the stream was still open when the collector gave up"
        );
        let status = final_status(&events);
        assert_eq!(status["status"]["state"], state, "{what}: {status}");
        if let Some(code) = code {
            let message = status["status"]["message"].as_str().unwrap();
            assert!(message.contains(code), "{what}: {message}");
        }
    }
}

/// S5. Thinking reaches the client once: a complete thought after its fragments is not sent
/// again, and a thought nothing streamed is.
#[test]
fn thinking_reaches_the_client_once() {
    if common::skip_without_host_support("thinking_reaches_the_client_once") {
        return;
    }
    let streamed = ProcessCapsule::new("think-process", "think").start();
    let events = collect_sse_events(&streamed.url(), STREAM_TIMEOUT);
    println!("think: {:?}", frame_kinds(&events));
    let thinking = frames_of(&events, "thinking");
    assert_eq!(thinking.len(), 2, "{thinking:#?}");
    assert_eq!(thinking[0]["text"], "step ");
    assert_eq!(thinking[1]["text"], "one");
    for frame in &thinking {
        assert_eq!(frame["final"], false, "{frame}");
    }

    let whole = ProcessCapsule::new("think-whole-process", "think-whole").start();
    let events = collect_sse_events(&whole.url(), STREAM_TIMEOUT);
    println!("think-whole: {:?}", frame_kinds(&events));
    let thinking = frames_of(&events, "thinking");
    assert_eq!(thinking.len(), 1, "{thinking:#?}");
    assert_eq!(thinking[0]["text"], "whole thought");
    assert_eq!(thinking[0]["final"], false);
}

/// S6. A process capsule's card advertises what its transport can do: cancellation, which every
/// transport supports, and streaming, which is its driver's answer.
#[test]
fn a_process_capsule_advertises_cancellation_and_streaming() {
    if common::skip_without_host_support("a_process_capsule_advertises_cancellation_and_streaming")
    {
        return;
    }
    let capsule = ProcessCapsule::new("card-process", "parity").start();
    let served = http_get(&capsule.url(), "/.well-known/agent-card.json");
    println!("process card: {served}");
    let card: Value = serde_json::from_str(&served).expect("the card is JSON");

    assert_eq!(card["capabilities"]["cancellation"], true, "{card}");
    assert_eq!(card["capabilities"]["streaming"], true, "{card}");
    assert!(
        card["serves"]["methods"]
            .as_array()
            .unwrap()
            .contains(&Value::from("tasks/cancel")),
        "the door still answers tasks/cancel: {card}"
    );
}
