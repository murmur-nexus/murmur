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

/// A hook a capsule under test declares: its artifact name, the event it binds to, and its
/// component. Packed with `commit_policy: none`, which is what an `on-inference` hook needs to
/// return an artifact.
struct DeclaredHook {
    name: &'static str,
    binding: &'static str,
    wasm: Vec<u8>,
}

impl DeclaredHook {
    /// Publish the hook into `home` and add its manifest entry to `entries`.
    fn declare(&self, home: &TempDir, artifacts: &Path, entries: &mut String) {
        publish(
            home,
            &common::hook_wat::create_hook_zip(
                artifacts,
                self.name,
                self.binding,
                "none",
                &self.wasm,
            ),
        );
        entries.push_str(&format!(
            "  - name: {}\n    version: {VERSION}\n    runtime: hook\n",
            self.name
        ));
    }
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

    /// This session's `trace.jsonl`: under `<project>/workdir/<session_id>/`, where a launch with
    /// no `--workdir` keeps its session.
    fn trace_path(&self) -> PathBuf {
        self._project
            .path()
            .join("workdir")
            .join(self.startup["session_id"].as_str().unwrap())
            .join("trace.jsonl")
    }

    /// The session's trace once the task has written `task_end`, polled because the stream's final
    /// frame can reach the client before the line reaches the file.
    fn trace_after_task_end(&self) -> Vec<Value> {
        let path = self.trace_path();
        let deadline = Instant::now() + STREAM_TIMEOUT;
        loop {
            let trace = read_trace(&path);
            if !trace_events(&trace, "task_end").is_empty() {
                return trace;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for task_end in {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(100));
        }
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

/// An `http` capsule driven by `server`, optionally declaring the echo tool and a hook.
fn http_capsule(
    server: &common::ScriptedServer,
    name: &str,
    tool: bool,
    hook: Option<&DeclaredHook>,
) -> Capsule {
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
    if let Some(hook) = hook {
        hook.declare(&home, artifacts.path(), &mut entries);
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

/// An `http` capsule whose driver streams its text in three chunks and needs no provider,
/// optionally declaring a hook.
fn streaming_http_capsule(name: &str, hook: Option<&DeclaredHook>) -> Capsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    publish(&home, &create_streaming_driver_artifact(artifacts.path()));
    let mut entries = format!(
        "  - name: {STREAMING_DRIVER}\n    version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:1\n      api_key: test-key\n"
    );
    if let Some(hook) = hook {
        hook.declare(&home, artifacts.path(), &mut entries);
    }
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: {name}\nversion: 0.1.0\nartifacts:\n{entries}\
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
    hook: Option<DeclaredHook>,
    /// `inference.driver.config`, as manifest lines under `driver:`.
    config: Option<String>,
    /// Extra environment for the `mur` process, for the debug inactivity override.
    env: Vec<(String, String)>,
    /// Extra manifest lines under `inference:`, ahead of `driver:`.
    inference: String,
}

impl ProcessCapsule {
    fn new(name: &str, profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            name: name.to_string(),
            tool: false,
            hook: None,
            config: None,
            env: Vec::new(),
            inference: String::new(),
        }
    }

    fn with_tool(mut self) -> Self {
        self.tool = true;
        self
    }

    fn with_hook(mut self, hook: DeclaredHook) -> Self {
        self.hook = Some(hook);
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

    /// Add `line` under `inference:`, indented as it will sit there: `"  max_session_tokens: 1000\n"`.
    fn inference_line(mut self, line: &str) -> Self {
        self.inference.push_str(line);
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
        if let Some(hook) = self.hook.as_ref() {
            hook.declare(&home, artifacts.path(), &mut entries);
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
        let inference = &self.inference;
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: {name}\nversion: 0.1.0\nartifacts:\n{entries}\
                 capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
                 {LIFECYCLE}\
                 inference:\n  transport: process\n{inference}  driver:\n    artifact: {PROCESS_DRIVER}\n{config}\
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

/// A provider answer that makes one `echo-tool` call.
fn echo_tool_call() -> String {
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
    .to_string()
}

fn tool_then_answer_server() -> common::ScriptedServer {
    common::ScriptedServer::start(vec![
        echo_tool_call(),
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
    /// The frame's `id:`, or `None` for a frame written without one.
    id: Option<u64>,
    event_type: String,
    data: String,
    /// When the frame's blank terminating line reached the client.
    received: Instant,
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

    read_sse_until_final(&mut reader)
}

/// Read SSE frames off `reader` until the first `status` frame with `"final":true`, or the end of
/// the stream.
fn read_sse_until_final(reader: &mut impl BufRead) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut id = None;
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
                    id,
                    event_type: event_type.clone(),
                    data: data.clone(),
                    received: Instant::now(),
                });
                if terminal {
                    break;
                }
            }
            id = None;
            event_type.clear();
            data.clear();
        } else if let Some(rest) = line.strip_prefix("id: ") {
            id = rest.parse().ok();
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

// ── Trace ─────────────────────────────────────────────────────────────────────

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn trace_events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
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
    let http = http_capsule(&server, "parity-http", true, None);
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
    let http = streaming_http_capsule("stream-http", None);
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

/// S6. A process capsule's card advertises what its transport can do: `tasks/cancel` on the door
/// extension, which every transport supports, and streaming, which is its driver's answer.
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

    assert_eq!(card["capabilities"]["streaming"], true, "{card}");
    assert!(
        common::card_door_methods(&card).contains(&"tasks/cancel"),
        "the door answers tasks/cancel: {card}"
    );
    assert_eq!(common::card_stream_frames(&card), PROCESS_FRAMES, "{card}");
}

/// The stream extension's `params.frames` of an http capsule that serves `message/stream`.
const HTTP_FRAMES: [&str; 9] = [
    "status",
    "artifact",
    "text",
    "thinking",
    "gap",
    "lagged",
    "connection-ack",
    "capsule-closed",
    "error",
];

/// The stream extension's `params.frames` of a `transport: process` capsule that serves
/// `message/stream`.
const PROCESS_FRAMES: [&str; 11] = [
    "status",
    "artifact",
    "text",
    "thinking",
    "tool-call-started",
    "tool-call-progress",
    "gap",
    "lagged",
    "connection-ack",
    "capsule-closed",
    "error",
];

/// Each capsule's card lists the frames its transport writes, and text, one tool call and an
/// answer on either transport write no frame its card leaves out.
#[test]
fn every_frame_a_capsule_writes_is_on_its_card() {
    if common::skip_without_host_support("every_frame_a_capsule_writes_is_on_its_card") {
        return;
    }
    let server = tool_then_answer_server();
    let http = http_capsule(&server, "frames-http", true, None);
    let process = ProcessCapsule::new("frames-process", "parity")
        .with_tool()
        .start();

    for (transport, url, expected) in [
        ("http", http.url(), HTTP_FRAMES.as_slice()),
        ("process", process.url(), PROCESS_FRAMES.as_slice()),
    ] {
        let served = http_get(&url, "/.well-known/agent-card.json");
        let card: Value = serde_json::from_str(&served).expect("the card is JSON");
        common::assert_a2a_agent_card(&card);
        let frames = common::card_stream_frames(&card);
        println!("{transport} card frames: {frames:?}");
        assert_eq!(frames, expected, "{transport}: {card}");

        let events = collect_sse_events(&url, STREAM_TIMEOUT);
        let observed: std::collections::BTreeSet<&str> =
            events.iter().map(|e| e.event_type.as_str()).collect();
        println!("{transport} observed event types: {observed:?}");
        assert!(!observed.is_empty(), "{transport}: no frames");
        for event_type in &observed {
            assert!(
                frames.contains(event_type),
                "{transport} wrote {event_type}, which its card does not list: {frames:?}"
            );
        }
    }
}

// ── A tool call being written ─────────────────────────────────────────────────

/// Assert `data` is an object with exactly `keys`, in that order.
fn assert_keys_in_order(data: &str, keys: &[&str]) {
    let parsed: Value = serde_json::from_str(data).expect("frame data is JSON");
    assert_eq!(
        parsed.as_object().map(|object| object.len()),
        Some(keys.len()),
        "{data}"
    );
    let positions: Vec<usize> = keys
        .iter()
        .map(|key| {
            data.find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("no {key} in {data}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "keys out of order in {data}"
    );
}

/// The `inference` records of a finished capsule's task, as `(turn, decision, tool_name)`.
fn inference_records(capsule: &Capsule) -> Vec<Value> {
    trace_events(&capsule.trace_after_task_end(), "inference")
        .into_iter()
        .map(|record| serde_json::json!([record["turn"], record["decision"], record["tool_name"]]))
        .collect()
}

/// Open `stream/watch` with `Last-Event-ID: last_event_id` and read it until a task's final
/// status, returning every frame after the connection's `connection-ack`. Every test that calls it
/// runs one task.
fn watch_until_final(addr: &str, last_event_id: u64) -> Vec<SseEvent> {
    let conn = common::idle_capsule::open_watch(addr, last_event_id);
    conn.set_read_timeout(Some(STREAM_TIMEOUT)).unwrap();
    let mut reader = BufReader::new(&conn);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => panic!("stream/watch closed before its headers ended"),
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {}
        }
    }
    let frames = read_sse_until_final(&mut reader);
    assert_eq!(
        frames.first().map(|frame| frame.event_type.as_str()),
        Some("connection-ack"),
        "{frames:#?}"
    );
    frames.into_iter().skip(1).collect()
}

/// `(id, kind)` of every frame, the shape a live stream and its replay are compared in.
fn ids_and_kinds(events: &[SseEvent]) -> Vec<(Option<u64>, String)> {
    events
        .iter()
        .map(|event| event.id)
        .zip(frame_kinds(events))
        .collect()
}

/// S7. A tool call the harness reports while the model writes it reaches the client as its start
/// and its size, before the `artifact` that answers it, and changes nothing the trace records.
#[test]
fn a_started_tool_call_streams_its_start_and_size_before_its_artifact() {
    if common::skip_without_host_support(
        "a_started_tool_call_streams_its_start_and_size_before_its_artifact",
    ) {
        return;
    }
    let capsule = ProcessCapsule::new("progress-process", "tool-progress")
        .with_tool()
        .start();
    let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    let kinds = frame_kinds(&events);
    println!("process: {kinds:?}");
    assert_eq!(
        kinds,
        [
            "status:working:inference turn 1",
            "tool-call-started",
            "tool-call-progress",
            "tool-call-progress",
            "artifact",
            "status:working:inference turn 2",
            "text:final",
            "status:completed",
        ]
    );

    let artifact = &frames_of(&events, "artifact")[0]["artifact"];
    let started = &frames_of(&events, "tool-call-started")[0];
    assert_eq!(started["tool_call_id"], "c1", "{started}");
    assert_eq!(
        started["tool_call_id"], artifact["tool_call_id"],
        "{artifact}"
    );
    assert_eq!(started["tool_name"], TOOL, "{started}");
    assert_eq!(started["tool_name"], artifact["tool_name"], "{artifact}");
    let progress = frames_of(&events, "tool-call-progress");
    assert_eq!(
        progress
            .iter()
            .map(|frame| frame["input_bytes"].clone())
            .collect::<Vec<_>>(),
        [12, 40]
    );
    for frame in &progress {
        assert_eq!(frame["tool_call_id"], "c1", "{frame}");
    }
    for event in &events {
        match event.event_type.as_str() {
            "tool-call-started" => {
                assert_keys_in_order(&event.data, &["id", "tool_call_id", "tool_name"])
            }
            "tool-call-progress" => {
                assert_keys_in_order(&event.data, &["id", "tool_call_id", "input_bytes"])
            }
            _ => {}
        }
    }

    let parity = ProcessCapsule::new("progress-parity", "parity")
        .with_tool()
        .start();
    collect_sse_events(&parity.url(), STREAM_TIMEOUT);
    let reported = inference_records(&capsule);
    println!("inference: {reported:?}");
    assert_eq!(reported, inference_records(&parity));
}

/// S8. A watcher that reconnects after the task replays the started call's frames under the ids
/// the live stream carried them with.
#[test]
fn a_reconnecting_watcher_replays_a_started_tool_call() {
    if common::skip_without_host_support("a_reconnecting_watcher_replays_a_started_tool_call") {
        return;
    }
    let capsule = ProcessCapsule::new("progress-replay", "tool-progress")
        .with_tool()
        .start();
    let live = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    assert!(live.iter().all(|event| event.id.is_some()), "{live:#?}");

    let replayed = watch_until_final(&capsule.url(), 0);
    println!("live:   {:?}", ids_and_kinds(&live));
    println!("replay: {:?}", ids_and_kinds(&replayed));
    assert_eq!(ids_and_kinds(&replayed), ids_and_kinds(&live));

    let working = live[0].id.expect("the first working status is numbered");
    let resumed = watch_until_final(&capsule.url(), working);
    println!("resumed after {working}: {:?}", ids_and_kinds(&resumed));
    assert_eq!(ids_and_kinds(&resumed), ids_and_kinds(&live[1..]));
    assert_eq!(resumed[0].event_type, "tool-call-started");
    for events in [&replayed, &resumed] {
        assert!(events.iter().all(|event| event.event_type != "gap"));
    }
}

/// S9. A 7000-byte input reported in 350 steps writes a handful of progress frames: none of the
/// task's frames is evicted from the replay buffer, and a watcher reading throughout is never
/// lagged.
#[test]
fn a_large_tool_input_writes_a_bounded_number_of_progress_frames() {
    if common::skip_without_host_support(
        "a_large_tool_input_writes_a_bounded_number_of_progress_frames",
    ) {
        return;
    }
    let capsule = ProcessCapsule::new("progress-large", "tool-progress-large")
        .with_tool()
        .start();
    let url = capsule.url();
    let watching = {
        let url = url.clone();
        thread::spawn(move || watch_until_final(&url, 0))
    };
    // The watcher attaches, and has written its `connection-ack`, before the task is submitted.
    thread::sleep(Duration::from_millis(500));
    let events = collect_sse_events(&url, STREAM_TIMEOUT);
    let watched = watching.join().expect("the watcher read to the end");

    println!("process: {:?}", frame_kinds(&events));
    let started: Vec<&SseEvent> = events
        .iter()
        .filter(|event| event.event_type == "tool-call-started")
        .collect();
    assert_eq!(started.len(), 1, "{events:#?}");
    let artifact = events
        .iter()
        .find(|event| event.event_type == "artifact")
        .expect("the call's artifact");
    let elapsed_ms = artifact
        .received
        .duration_since(started[0].received)
        .as_millis() as u64;
    let sizes: Vec<u64> = frames_of(&events, "tool-call-progress")
        .iter()
        .map(|frame| frame["input_bytes"].as_u64().unwrap())
        .collect();
    let ceiling = 2 + elapsed_ms.div_ceil(250);
    println!(
        "{} tool-call-progress frames over {elapsed_ms} ms (ceiling {ceiling}): {sizes:?}",
        sizes.len()
    );
    assert!(
        (1..=ceiling).contains(&(sizes.len() as u64)),
        "{} progress frames over {elapsed_ms} ms",
        sizes.len()
    );
    assert!(sizes.windows(2).all(|pair| pair[0] < pair[1]), "{sizes:?}");
    assert_eq!(sizes.last(), Some(&7000));

    assert!(
        watched.iter().all(|event| event.event_type != "lagged"),
        "{:?}",
        frame_kinds(&watched)
    );
    assert_eq!(ids_and_kinds(&watched), ids_and_kinds(&events));

    let replayed = watch_until_final(&url, 0);
    assert!(replayed.iter().all(|event| event.event_type != "gap"));
    assert_eq!(replayed[0].id, events[0].id);
    assert_eq!(
        frame_kinds(&replayed[..1]),
        ["status:working:inference turn 1"]
    );
}

/// The value the `tool-progress` profile's call carries as its input, and nothing else in the run.
const STARTED_CALL_INPUT: &str = "QXZJ-WKVR-MPLT-YNGH";

/// S10. A started call's input reaches the trace and no frame of the stream, live or replayed.
/// Every four-byte piece of the input's value is looked for, so a fragment of it is caught as
/// well as the whole.
#[test]
fn no_frame_carries_any_part_of_a_started_call_s_input() {
    if common::skip_without_host_support("no_frame_carries_any_part_of_a_started_call_s_input") {
        return;
    }
    let capsule = ProcessCapsule::new("progress-input", "tool-progress")
        .with_tool()
        .start();
    let live = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    let replayed = watch_until_final(&capsule.url(), 0);
    assert_eq!(
        frame_kinds(&live)[1..5],
        [
            "tool-call-started",
            "tool-call-progress",
            "tool-call-progress",
            "artifact"
        ]
    );

    let trace = capsule.trace_after_task_end();
    let calls = trace_events(&trace, "tool_call");
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert_eq!(calls[0]["input"]["msg"], STARTED_CALL_INPUT, "{}", calls[0]);

    let pieces: Vec<&str> = (0..=STARTED_CALL_INPUT.len() - 4)
        .map(|at| &STARTED_CALL_INPUT[at..at + 4])
        .collect();
    for event in live.iter().chain(&replayed) {
        for piece in &pieces {
            assert!(
                !event.data.contains(piece),
                "a {} frame carries {piece:?} of the call's input: {}",
                event.event_type,
                event.data
            );
        }
    }
}

/// The `on-inference` hook the hook scenarios declare, and what it returns every turn.
const HOOK: &str = "review-hook";
const HOOK_PAYLOAD: &str = r#"{"reviewed":true}"#;

fn inference_hook() -> DeclaredHook {
    DeclaredHook {
        name: HOOK,
        binding: "on-inference",
        wasm: common::hook_wat::artifact_hook_wasm("on-inference", HOOK_PAYLOAD),
    }
}

/// The frames of `events` that carry the declared hook's artifact.
fn hook_artifacts(events: &[SseEvent]) -> Vec<Value> {
    artifacts_from(events, HOOK)
}

/// The frames of `events` that carry an artifact from the hook named `hook`.
fn artifacts_from(events: &[SseEvent], hook: &str) -> Vec<Value> {
    frames_of(events, "artifact")
        .into_iter()
        .map(|frame| frame["artifact"].clone())
        .filter(|artifact| artifact["tool_name"] == hook)
        .collect()
}

/// Assert `artifact` is the hook's payload, carried the way the operator's own hook speaks:
/// unfenced, and with none of a tool call's keys.
fn assert_is_hook_artifact(transport: &str, artifact: &Value) {
    assert_eq!(artifact["tool_name"], HOOK, "{transport}: {artifact}");
    assert_eq!(artifact["content"], HOOK_PAYLOAD, "{transport}: {artifact}");
    for key in ["fence_source", "tool_call_id", "duration_ms", "exit_code"] {
        assert_eq!(
            artifact[key],
            Value::Null,
            "{transport}: {key} in {artifact}"
        );
    }
    assert_eq!(artifact["is_error"], false, "{transport}: {artifact}");
    assert_eq!(artifact["truncated"], false, "{transport}: {artifact}");
}

/// An `on-inference` hook's artifact for the attempt's last turn reaches the client on both
/// transports, after that turn's work and before its fallback text; the tool-calling turn's
/// artifact reaches neither.
#[test]
fn an_inference_hook_artifact_writes_the_same_frames() {
    if common::skip_without_host_support("an_inference_hook_artifact_writes_the_same_frames") {
        return;
    }
    let server = tool_then_answer_server();
    let http = http_capsule(&server, "hook-http", true, Some(&inference_hook()));
    let http_events = collect_sse_events(&http.url(), STREAM_TIMEOUT);

    let process = ProcessCapsule::new("hook-process", "parity")
        .with_tool()
        .with_hook(inference_hook())
        .start();
    let process_events = collect_sse_events(&process.url(), STREAM_TIMEOUT);

    assert_same_frames(
        &http_events,
        &process_events,
        &[
            "status:working:inference turn 1",
            "artifact",
            "status:working:inference turn 2",
            "artifact",
            "text:final",
            "status:completed",
        ],
    );

    let mut forwarded = Vec::new();
    for (transport, events) in [("http", &http_events), ("process", &process_events)] {
        let artifacts = frames_of(events, "artifact");
        assert_eq!(artifacts[0]["artifact"]["tool_name"], TOOL, "{transport}");
        let hooks = hook_artifacts(events);
        assert_eq!(hooks.len(), 1, "{transport}: {hooks:#?}");
        assert_is_hook_artifact(transport, &artifacts[1]["artifact"]);
        assert_eq!(
            final_status(events)["status"]["response"],
            "PARITY-ANSWER",
            "{transport}"
        );
        forwarded.push(artifacts[1]["artifact"].clone());
    }
    assert_eq!(forwarded[0], forwarded[1], "the hook's frame differs");
}

/// A streamed answer's cursor removal closes the text the client was streamed before the
/// hook's artifact arrives, on both transports.
#[test]
fn a_streamed_answer_places_the_hook_artifact_after_the_cursor_removal() {
    if common::skip_without_host_support(
        "a_streamed_answer_places_the_hook_artifact_after_the_cursor_removal",
    ) {
        return;
    }
    let http = streaming_http_capsule("hook-stream-http", Some(&inference_hook()));
    let http_events = collect_sse_events(&http.url(), STREAM_TIMEOUT);

    let process = ProcessCapsule::new("hook-stream-process", "stream")
        .with_hook(inference_hook())
        .start();
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
            "artifact",
            "status:completed",
        ],
    );

    for (transport, events) in [("http", &http_events), ("process", &process_events)] {
        let texts = frames_of(events, "text");
        assert_eq!(texts[3]["text"], "", "{transport}: the cursor removal");
        assert_is_hook_artifact(transport, &frames_of(events, "artifact")[0]["artifact"]);
    }
}

/// A turn the harness fails ran its `on-inference` hooks, and forwards none of what they
/// returned: the attempt did not complete.
#[test]
fn a_failed_turn_forwards_no_hook_artifact() {
    if common::skip_without_host_support("a_failed_turn_forwards_no_hook_artifact") {
        return;
    }
    let capsule = ProcessCapsule::new("hook-fail-process", "text-then-fail")
        .with_hook(inference_hook())
        .start();
    let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    let kinds = frame_kinds(&events);
    println!("process: {kinds:?}");

    assert!(
        kinds.contains(&"status:working:inference turn 1".to_string()),
        "the failed turn opened: {kinds:?}"
    );
    assert!(
        frames_of(&events, "artifact").is_empty(),
        "a failed attempt forwards no artifact: {kinds:?}"
    );
    assert_eq!(final_status(&events)["status"]["state"], "failed");
}

/// The `on-inference` hook the `run-inference` scenarios declare: it calls `run-inference` once and
/// returns whatever string the call produced as its artifact.
const INFERRING_HOOK: &str = "inferring-hook";

fn inferring_hook() -> DeclaredHook {
    DeclaredHook {
        name: INFERRING_HOOK,
        binding: "on-inference",
        wasm: common::hook_wat::run_inference_then_artifact_hook_wasm(),
    }
}

/// What `run-inference` answers a hook with under `transport: process`.
const PROCESS_TRANSPORT_REFUSAL: &str = "run-inference is not available under \
     inference.transport: process: the harness runs the model, and this capsule has no inference \
     driver the runtime can call";

/// Print what the `run-inference` scenarios assert on, before any assertion can stop the test.
fn print_inference_evidence(events: &[SseEvent], trace: &[Value]) {
    println!("frames: {:?}", frame_kinds(events));
    for artifact in artifacts_from(events, INFERRING_HOOK) {
        println!("hook artifact payload: {}", artifact["content"]);
    }
    for line in trace_events(trace, "inference") {
        println!("inference: {line}");
    }
    for line in trace_events(trace, "spend_ceiling_reached") {
        println!("spend_ceiling_reached: {line}");
    }
}

/// Under `transport: http` a hook's `run-inference` reaches the provider, and its `inference` line
/// is written on the hook's turn, tagged with the hook's origin, ahead of that turn's own line.
#[test]
fn a_hook_s_run_inference_is_traced_on_its_turn_ahead_of_the_turn_on_http() {
    if common::skip_without_host_support(
        "a_hook_s_run_inference_is_traced_on_its_turn_ahead_of_the_turn_on_http",
    ) {
        return;
    }
    // The agent's turn is dispatched before its `on-inference` hooks run, so each turn's answer
    // precedes the hook's completion.
    let server = common::ScriptedServer::start(vec![
        echo_tool_call(),
        common::idle_capsule::end_turn(2, "HOOK-COMPLETION"),
        common::idle_capsule::end_turn(3, "PARITY-ANSWER"),
        common::idle_capsule::end_turn(4, "HOOK-COMPLETION"),
    ]);
    let capsule = http_capsule(&server, "hook-infer-http", true, Some(&inferring_hook()));
    let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    let trace = capsule.trace_after_task_end();
    print_inference_evidence(&events, &trace);

    let status = final_status(&events);
    assert_eq!(status["status"]["state"], "completed", "{status}");
    assert_eq!(status["status"]["response"], "PARITY-ANSWER", "{status}");
    let hooks = artifacts_from(&events, INFERRING_HOOK);
    assert_eq!(hooks.len(), 1, "{hooks:#?}");
    assert_eq!(hooks[0]["content"], "HOOK-COMPLETION", "{}", hooks[0]);

    let inference = trace_events(&trace, "inference");
    let lines: Vec<(u64, Option<&str>)> = inference
        .iter()
        .map(|line| (line["turn"].as_u64().unwrap(), line["origin"].as_str()))
        .collect();
    let hook_origin = format!("hook:{INFERRING_HOOK}");
    assert_eq!(
        lines,
        [
            (0, Some(hook_origin.as_str())),
            (0, None),
            (1, Some(hook_origin.as_str())),
            (1, None),
        ],
        "{inference:#?}"
    );
    assert_eq!(inference[1]["decision"], "tool_call", "{}", inference[1]);
}

/// Under `transport: process` a hook's `run-inference` is answered with an error naming the
/// transport, before anything is sent or admitted: no hook `inference` line and no
/// `spend_ceiling_reached` line, under a session ceiling far below the call's 8192-token output
/// reservation.
#[test]
fn a_hook_s_run_inference_under_process_transport_is_refused_and_spends_nothing() {
    if common::skip_without_host_support(
        "a_hook_s_run_inference_under_process_transport_is_refused_and_spends_nothing",
    ) {
        return;
    }
    let capsule = ProcessCapsule::new("hook-infer-process", "parity")
        .with_tool()
        .with_hook(inferring_hook())
        .inference_line("  max_session_tokens: 1000\n")
        .start();
    let events = collect_sse_events(&capsule.url(), STREAM_TIMEOUT);
    let trace = capsule.trace_after_task_end();
    print_inference_evidence(&events, &trace);

    let inference = trace_events(&trace, "inference");
    let turns: Vec<(u64, bool)> = inference
        .iter()
        .map(|line| (line["turn"].as_u64().unwrap(), line.get("origin").is_some()))
        .collect();
    assert_eq!(turns, [(0, false), (1, false)], "{inference:#?}");
    let refusals = trace_events(&trace, "spend_ceiling_reached");
    assert!(refusals.is_empty(), "{refusals:#?}");
    let task_end = trace_events(&trace, "task_end");
    assert_eq!(task_end.len(), 1, "{task_end:#?}");
    assert_eq!(task_end[0]["exit_status"], "ok", "{}", task_end[0]);

    let status = final_status(&events);
    assert_eq!(status["status"]["state"], "completed", "{status}");
    assert_eq!(status["status"]["response"], "PARITY-ANSWER", "{status}");
    let hooks = artifacts_from(&events, INFERRING_HOOK);
    assert_eq!(hooks.len(), 1, "{hooks:#?}");
    assert_eq!(
        hooks[0]["content"], PROCESS_TRANSPORT_REFUSAL,
        "{}",
        hooks[0]
    );
}
