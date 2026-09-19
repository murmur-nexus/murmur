//! The streaming protocol reference page, held against the wire a launched capsule writes: the
//! frames a task writes, the `lagged` frame a connection that falls behind is sent, and
//! `mur watch`'s report of a connection that ends without `capsule-closed`.
//!
//! Every capsule here is its own `mur run` process: `mur watch` is attached to it from outside,
//! and the lost-connection case needs a process to kill.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    process::{Child, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The tool the scripted provider asks for. The capsule declares no tools, so the call fails at
/// dispatch and still writes an `artifact` frame.
const UNDECLARED_TOOL: &str = "no-such-tool";

fn page() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/content/reference/streaming-protocol.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn_response(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn tool_call_response(id: &str, tool_use_id: &str, name: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": {}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn undeclared_tool_then_end_turn() -> common::ScriptedServer {
    common::ScriptedServer::start(vec![
        tool_call_response("msg_1", "toolu_1", UNDECLARED_TOOL),
        end_turn_response("msg_2", "done"),
    ])
}

// ── A capsule running as its own process ──────────────────────────────────────

struct Capsule {
    child: Child,
    _home: TempDir,
    _project: TempDir,
    /// The `mur run --json` startup line.
    startup: Value,
    /// Every line the capsule has written to stderr so far.
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Capsule {
    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn pid(&self) -> u32 {
        self.startup["pid"].as_u64().unwrap() as u32
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a queue + sleep agent capsule with no shell allowlist and no tools, and blocks until it
/// has printed its address.
fn start_capsule(server: &common::ScriptedServer, name: &str) -> Capsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", driver.to_str().unwrap()])
        .assert()
        .success();

    let endpoint = &server.endpoint;
    let project = tempfile::tempdir().unwrap();
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             \x20   gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n\
             inference:\n  transport: http\n  model: test-model\n  \
             driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["run", "--manifest"])
        .arg(&manifest)
        .arg("--json")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mur run should start");

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
    let stderr_lines = Arc::new(Mutex::new(Vec::new()));
    let stderr = child.stderr.take().unwrap();
    {
        let stderr_lines = Arc::clone(&stderr_lines);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
                stderr_lines.lock().unwrap().push(line);
            }
        });
    }

    let startup = match startup_rx.recv_timeout(Duration::from_secs(120)) {
        Ok(startup) => startup,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for the capsule to print its address");
        }
    };

    Capsule {
        child,
        _home: home,
        _project: project,
        startup,
        stderr: stderr_lines,
    }
}

/// Posts one JSON-RPC request and returns the parsed response body, or `null` when it is not JSON.
fn rpc(addr: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).ok();
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap_or(Value::Null)
}

fn submit(addr: &str, message_id: &str, text: &str) -> String {
    let params = json!({"message": {
        "messageId": message_id,
        "role": "user",
        "parts": [{"text": text}]
    }});
    let response = rpc(addr, "message/send", params);
    response["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the door did not accept the task: {response}"))
        .to_string()
}

// ── Raw SSE reading ───────────────────────────────────────────────────────────

/// One blank-line-delimited block of the SSE body, every line kept.
#[derive(Debug)]
struct Block {
    lines: Vec<String>,
}

impl Block {
    fn field(&self, name: &str) -> Option<&str> {
        let prefix = format!("{name}: ");
        self.lines
            .iter()
            .find_map(|line| line.strip_prefix(&prefix))
    }

    fn is_comment(&self) -> bool {
        self.lines.iter().all(|line| line.starts_with(':'))
    }
}

/// Opens `stream/watch` with no `Last-Event-ID` and reads blocks until `done` accepts one or
/// `timeout` passes. Comment lines are kept, as their own blocks.
fn watch_until(addr: &str, timeout: Duration, done: impl Fn(&Block) -> bool) -> Vec<Block> {
    let stream = TcpStream::connect(addr).expect("should connect to capsule");
    let mut reader = SseReader::open(stream, addr, WATCH_BODY);
    let (blocks, end) = reader.read_until(timeout, done);
    assert_eq!(
        end,
        ReadEnd::Accepted,
        "the watched stream did not reach the expected frame in {timeout:?}; blocks were: {blocks:?}"
    );
    blocks
}

const WATCH_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"stream/watch","params":{}}"#;

#[derive(Debug, PartialEq)]
enum ReadEnd {
    /// The `done` predicate accepted a block.
    Accepted,
    /// The server closed the connection.
    Closed,
    TimedOut,
}

/// An SSE response read block by block, across as many `read_until` calls as the test needs.
struct SseReader {
    stream: TcpStream,
    pending: Vec<u8>,
    /// How far into `pending` has already been searched for a newline.
    scanned: usize,
    in_headers: bool,
    current: Vec<String>,
}

impl SseReader {
    /// Sends `body` as a JSON-RPC POST on `stream`, reading nothing yet.
    fn open(mut stream: TcpStream, addr: &str, body: &str) -> Self {
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        Self {
            stream,
            pending: Vec::new(),
            scanned: 0,
            in_headers: true,
            current: Vec::new(),
        }
    }

    fn read_until(
        &mut self,
        timeout: Duration,
        done: impl Fn(&Block) -> bool,
    ) -> (Vec<Block>, ReadEnd) {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 64 * 1024];
        let mut blocks = Vec::new();

        loop {
            while let Some(offset) = self.pending[self.scanned..]
                .iter()
                .position(|b| *b == b'\n')
            {
                let pos = self.scanned + offset;
                self.scanned = 0;
                let raw: Vec<u8> = self.pending.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                    .trim_end_matches('\r')
                    .to_string();
                if self.in_headers {
                    self.in_headers = !line.is_empty();
                    continue;
                }
                if !line.is_empty() {
                    self.current.push(line);
                    continue;
                }
                if self.current.is_empty() {
                    continue;
                }
                let block = Block {
                    lines: std::mem::take(&mut self.current),
                };
                let finished = done(&block);
                blocks.push(block);
                if finished {
                    return (blocks, ReadEnd::Accepted);
                }
            }
            self.scanned = self.pending.len();

            if Instant::now() >= deadline {
                return (blocks, ReadEnd::TimedOut);
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return (blocks, ReadEnd::Closed),
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return (blocks, ReadEnd::Closed),
            }
        }
    }
}

fn object_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Every frame type, key and id rule a real task produces is on the page.
#[test]
fn the_page_covers_every_frame_a_task_writes() {
    let server = undeclared_tool_then_end_turn();
    let capsule = start_capsule(&server, "streaming-protocol-doc");
    let addr = capsule.url();

    let task_id = Arc::new(Mutex::new(None::<String>));
    let submitter = {
        let addr = addr.clone();
        let task_id = Arc::clone(&task_id);
        thread::spawn(move || {
            // Submitted once the watch is attached, so the task's frames arrive live as well as
            // through the replay.
            thread::sleep(Duration::from_millis(500));
            *task_id.lock().unwrap() = Some(submit(&addr, "msg-1", "call a tool"));
        })
    };

    let blocks = watch_until(&addr, Duration::from_secs(120), |block| {
        block.field("event") == Some("status")
            && block
                .field("data")
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .is_some_and(|data| data["final"] == json!(true))
    });
    submitter.join().unwrap();
    let task_id = task_id.lock().unwrap().clone().expect("task was submitted");

    let page = page();
    let frames: Vec<&Block> = blocks.iter().filter(|block| !block.is_comment()).collect();

    // A heartbeat is a comment standing alone, never part of a frame.
    for block in &blocks {
        if block.lines.iter().any(|line| line.starts_with(':')) {
            assert_eq!(block.lines, vec![":heartbeat".to_string()], "{block:?}");
        }
    }

    let mut types = BTreeSet::new();
    let mut id_less = BTreeSet::new();
    for frame in &frames {
        let event = frame
            .field("event")
            .unwrap_or_else(|| panic!("frame without an event line: {frame:?}"));
        types.insert(event.to_string());
        if frame.field("id").is_none() {
            id_less.insert(event.to_string());
        }

        let data: Value = serde_json::from_str(frame.field("data").unwrap_or("null"))
            .unwrap_or_else(|e| panic!("data is not JSON ({e}): {frame:?}"));
        for key in object_keys(&data) {
            assert!(
                page.contains(&format!("`{key}`")),
                "`{event}` key `{key}` is not on the page"
            );
        }
        for (object, key) in [("status", "status"), ("artifact", "artifact")] {
            for nested in object_keys(&data[key]) {
                assert!(
                    page.contains(&format!("`{object}.{nested}`"))
                        || page.contains(&format!("`{nested}`")),
                    "`{object}` key `{nested}` is not on the page"
                );
            }
        }
    }

    for event in &types {
        assert!(
            page.contains(&format!("| [`{event}`](#event-{event}) |")),
            "event type `{event}` is not in the page's endpoint table"
        );
    }
    for expected in ["connection-ack", "status", "artifact"] {
        assert!(
            types.contains(expected),
            "no `{expected}` frame seen: {types:?}"
        );
    }

    let allowed: HashSet<&str> = ["connection-ack", "gap", "lagged"].into_iter().collect();
    assert!(
        id_less.contains("connection-ack")
            && id_less.iter().all(|event| allowed.contains(event.as_str())),
        "frames without an id line were {id_less:?}"
    );
    for event in &id_less {
        assert!(
            page.contains(&format!("| `{event}` | no |")),
            "the page does not list `{event}` as a frame without an id"
        );
    }

    let artifact = frames
        .iter()
        .filter(|frame| frame.field("event") == Some("artifact"))
        .filter_map(|frame| serde_json::from_str::<Value>(frame.field("data")?).ok())
        .find(|data| data["id"] == json!(task_id))
        .expect("the task wrote an artifact frame");
    assert_eq!(artifact["artifact"]["tool_name"], UNDECLARED_TOOL);
    assert_eq!(artifact["artifact"]["is_error"], true);
    assert_eq!(artifact["artifact"]["fence_source"], Value::Null);
}

#[test]
fn the_page_names_every_state_and_mode() {
    let page = page();
    for state in [
        "working",
        "input-required",
        "completed",
        "failed",
        "canceled",
        "rejected",
    ] {
        assert!(
            page.contains(&format!("| `{state}` |")),
            "state `{state}` has no row"
        );
    }
    for mode in ["`stateless`", "`threaded`"] {
        assert!(page.contains(mode), "conversation mode {mode} is not named");
    }
}

/// A capsule that dies under an attached `mur watch` is reported as a lost connection, not as a
/// closed capsule.
#[test]
fn watch_reports_a_lost_connection() {
    let server = undeclared_tool_then_end_turn();
    let capsule = start_capsule(&server, "watch-lost-connection");

    let mut watch = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["watch", "--url", &capsule.url()])
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mur watch should start");

    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stderr = watch.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });

    let mut stderr_lines = Vec::new();
    let banner = line_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("mur watch printed no banner");
    assert!(banner.contains("watching"), "{banner}");
    stderr_lines.push(banner);

    let _ = std::process::Command::new("kill")
        .args(["-9", &capsule.pid().to_string()])
        .status();

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = watch.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = watch.kill();
            panic!("mur watch was still running 30 seconds after the capsule was killed");
        }
        thread::sleep(Duration::from_millis(50));
    };
    while let Ok(line) = line_rx.recv_timeout(Duration::from_secs(2)) {
        stderr_lines.push(line);
    }
    let stderr = stderr_lines.join("\n");

    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("E-IO-003"), "{stderr}");
    assert!(stderr.contains("connection to"), "{stderr}");
    assert!(stderr.contains("lost"), "{stderr}");
    assert!(!stderr.contains("capsule closed"), "{stderr}");
}

// ── A connection that falls behind ────────────────────────────────────────────

/// The size of the first task's reply. Its `text` frame alone is larger than a loopback socket's
/// send and receive buffers together, so a client that reads nothing leaves the connection's
/// handler blocked part-way through writing it.
const BLOCKING_REPLY_BYTES: usize = 6 * 1024 * 1024;

/// Tasks run after the blocking reply. Each writes several frames, so together they overflow the
/// blocked connection's queue of 128 while it cannot drain.
const FLOOD_TASKS: usize = 60;

/// One reply of [`BLOCKING_REPLY_BYTES`], then [`FLOOD_TASKS`] short ones.
fn blocking_reply_then_flood() -> common::ScriptedServer {
    let reply = "lorem ipsum ".repeat(BLOCKING_REPLY_BYTES / "lorem ipsum ".len());
    let mut responses = vec![end_turn_response("msg_block", &reply)];
    responses.extend((0..FLOOD_TASKS).map(|i| end_turn_response(&format!("msg_{i}"), "ok")));
    common::ScriptedServer::start(responses)
}

/// Connects with the smallest receive buffer the kernel allows, so the server's writes block as
/// soon as its own send buffer is full.
fn connect_slow_reader(addr: &str) -> TcpStream {
    let addr = std::net::ToSocketAddrs::to_socket_addrs(addr)
        .unwrap()
        .find(std::net::SocketAddr::is_ipv4)
        .expect("the capsule listens on IPv4");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let stream = runtime.block_on(async {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        socket.connect(addr).await.unwrap().into_std().unwrap()
    });
    stream.set_nonblocking(false).unwrap();
    stream
}

/// Once the provider has seen the blocking task's request, submits [`FLOOD_TASKS`] tasks one at a
/// time and waits for the last to complete. Returns the last task's id.
fn flood(server: &common::ScriptedServer, addr: &str) -> String {
    let wait_for_requests = |count: usize| {
        let deadline = Instant::now() + Duration::from_secs(120);
        while server.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "the provider saw {} of {count} requests",
                server.requests().len()
            );
            thread::sleep(Duration::from_millis(20));
        }
    };
    let mut last = String::new();
    for i in 0..FLOOD_TASKS {
        wait_for_requests(i + 1);
        last = submit(addr, &format!("flood-{i}"), "go");
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let got = rpc(addr, "tasks/get", json!({"id": last}));
        if got["result"]["status"]["state"] == "completed" {
            return last;
        }
        assert!(
            Instant::now() < deadline,
            "the last task never completed: {got}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The `missed` count of every `lagged` frame in `blocks`, asserting each is id-less.
fn lagged_counts(blocks: &[Block]) -> Vec<u64> {
    blocks
        .iter()
        .filter(|block| block.field("event") == Some("lagged"))
        .map(|block| {
            assert_eq!(
                block.field("id"),
                None,
                "a lagged frame carries an id: {block:?}"
            );
            let data: Value = serde_json::from_str(block.field("data").unwrap()).unwrap();
            assert_eq!(object_keys(&data), vec!["missed"], "{block:?}");
            data["missed"].as_u64().expect("missed is a u64")
        })
        .collect()
}

/// The counts the capsule wrote to stderr in lines starting with `prefix`, once they add up to
/// `expected_total` or ten seconds have passed.
fn reported_lags(capsule: &Capsule, prefix: &str, expected_total: u64) -> Vec<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let counts: Vec<u64> = capsule
            .stderr
            .lock()
            .unwrap()
            .iter()
            .filter_map(|line| {
                line.strip_prefix(prefix)?
                    .strip_suffix(" events")?
                    .parse()
                    .ok()
            })
            .collect();
        if counts.iter().sum::<u64>() >= expected_total || Instant::now() >= deadline {
            return counts;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn is_final_status_of(block: &Block, task_id: &str) -> bool {
    block.field("event") == Some("status")
        && block
            .field("data")
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .is_some_and(|data| data["final"] == json!(true) && data["id"] == json!(task_id))
}

/// A fresh `stream/watch` replays the session's buffer through `last_task`'s final status without a
/// `lagged` frame in it.
fn assert_replay_has_no_lagged_frame(addr: &str, last_task: &str) {
    let replay = watch_until(addr, Duration::from_secs(120), |block| {
        is_final_status_of(block, last_task)
    });
    assert!(
        replay
            .iter()
            .all(|block| block.field("event") != Some("lagged")),
        "a lagged frame was replayed"
    );
}

/// A `stream/watch` client that stops reading while a capsule writes more than 128 frames is sent
/// one id-less `lagged` frame per loss, carrying the count the runtime reported, and keeps
/// receiving on the same connection.
#[test]
fn a_lagging_watch_connection_is_told_how_many_frames_it_lost() {
    let server = blocking_reply_then_flood();
    let capsule = start_capsule(&server, "lagged-watch");
    let addr = capsule.url();

    let mut slow = SseReader::open(connect_slow_reader(&addr), &addr, WATCH_BODY);
    let (_, end) = slow.read_until(Duration::from_secs(30), |block| {
        block.field("event") == Some("connection-ack")
    });
    assert_eq!(end, ReadEnd::Accepted, "no connection-ack");

    let blocking_task = submit(&addr, "block", "reply at length");
    let last_task = flood(&server, &addr);

    let (blocks, end) = slow.read_until(Duration::from_secs(120), |block| {
        is_final_status_of(block, &last_task)
    });
    assert_eq!(
        end,
        ReadEnd::Accepted,
        "the watch did not reach the last task's final status"
    );

    let sent = lagged_counts(&blocks);
    assert!(!sent.is_empty(), "no lagged frame reached the connection");
    let reported = reported_lags(
        &capsule,
        "[capsule-runtime] stream/watch: SSE broadcast lagged by ",
        sent.iter().sum(),
    );
    assert_eq!(
        sent, reported,
        "the lagged frames' counts are not the counts the runtime reported"
    );
    assert!(
        !blocks
            .iter()
            .any(|block| is_final_status_of(block, &blocking_task)),
        "the blocking task's final status was delivered, so nothing was lost before it"
    );

    assert_replay_has_no_lagged_frame(&addr, &last_task);
}

/// A `message/stream` client that stops reading while a capsule writes more than 128 frames is sent
/// a `lagged` frame carrying the count the runtime reported, then closes on the first `final`
/// status it is delivered — here another task's, because its own was among the frames lost.
#[test]
fn a_lagging_message_stream_connection_is_told_how_many_frames_it_lost() {
    let server = blocking_reply_then_flood();
    let capsule = start_capsule(&server, "lagged-message-stream");
    let addr = capsule.url();

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {"message": {
            "messageId": "block",
            "role": "user",
            "parts": [{"text": "reply at length"}]
        }}
    })
    .to_string();
    let mut slow = SseReader::open(connect_slow_reader(&addr), &addr, &body);
    let last_task = flood(&server, &addr);

    let (blocks, end) = slow.read_until(Duration::from_secs(120), |_| false);
    assert_eq!(end, ReadEnd::Closed, "the connection did not close");

    let sent = lagged_counts(&blocks);
    assert!(!sent.is_empty(), "no lagged frame reached the connection");
    let reported = reported_lags(
        &capsule,
        "[capsule-runtime] SSE broadcast lagged by ",
        sent.iter().sum(),
    );
    assert_eq!(
        sent, reported,
        "the lagged frames' counts are not the counts the runtime reported"
    );

    let own_task = blocks
        .iter()
        .find_map(|block| {
            let data: Value = serde_json::from_str(block.field("data")?).ok()?;
            data["id"].as_str().map(str::to_string)
        })
        .expect("the connection delivered a frame of its own task before blocking");
    let last = blocks.last().unwrap();
    assert_eq!(last.field("event"), Some("status"), "{last:?}");
    let last_data: Value = serde_json::from_str(last.field("data").unwrap()).unwrap();
    assert_eq!(
        last_data["final"],
        json!(true),
        "the connection closed on {last:?}"
    );
    assert_ne!(
        last_data["id"],
        json!(own_task),
        "the connection's own final status was delivered, so nothing was lost before it"
    );

    assert_replay_has_no_lagged_frame(&addr, &last_task);
}
