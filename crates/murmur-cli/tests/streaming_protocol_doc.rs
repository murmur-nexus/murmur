//! The streaming protocol reference page, held against the wire a launched capsule writes: the
//! events and frames a task writes, the `lagged` frames a connection that falls behind is sent,
//! `mur watch`'s report of a connection that ends without `capsule-closed`, and the lines it
//! prints.
//!
//! Every capsule here is its own `mur run` process: `mur watch` is attached to it from outside,
//! and the lost-connection case needs a process to kill.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
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
use common::stream::{self, Event};
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
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        "role": "ROLE_USER",
        "parts": [{"text": text, "mediaType": "text/plain"}]
    }});
    let response = rpc(addr, "SendMessage", params);
    response["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the door did not accept the task: {response}"))
        .to_string()
}

// ── Raw SSE reading ───────────────────────────────────────────────────────────

/// Opens `stream/watch` with no `Last-Event-ID` and reads events until `done` accepts one or
/// `timeout` passes.
fn watch_until(addr: &str, timeout: Duration, done: impl Fn(&Event) -> bool) -> Vec<Event> {
    let stream = TcpStream::connect(addr).expect("should connect to capsule");
    let mut reader = SseReader::open(stream, addr, WATCH_BODY, &[]);
    let (events, end) = reader.read_until(timeout, done);
    assert_eq!(
        end,
        ReadEnd::Accepted,
        "the watched stream did not reach the expected event in {timeout:?}; events were: {events:#?}"
    );
    events
}

const WATCH_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"stream/watch","params":{}}"#;

#[derive(Debug, PartialEq)]
enum ReadEnd {
    /// The `done` predicate accepted an event.
    Accepted,
    /// The server closed the connection.
    Closed,
    TimedOut,
}

/// An SSE response read event by event, across as many `read_until` calls as the test needs.
struct SseReader {
    stream: TcpStream,
    pending: Vec<u8>,
    /// How far into `pending` has already been searched for a newline.
    scanned: usize,
    in_headers: bool,
    current: Vec<String>,
}

impl SseReader {
    /// Sends `body` as a JSON-RPC POST on `stream` with `headers` besides the usual ones, reading
    /// nothing yet.
    fn open(mut stream: TcpStream, addr: &str, body: &str, headers: &[(&str, &str)]) -> Self {
        let extra: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let request = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\n{extra}Content-Length: {}\r\n\r\n{body}",
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
        done: impl Fn(&Event) -> bool,
    ) -> (Vec<Event>, ReadEnd) {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 64 * 1024];
        let mut events = Vec::new();

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
                let block = std::mem::take(&mut self.current);
                // A heartbeat block parses to no event.
                for event in stream::parse(&format!("{}\n\n", block.join("\n"))) {
                    let finished = done(&event);
                    events.push(event);
                    if finished {
                        return (events, ReadEnd::Accepted);
                    }
                }
            }
            self.scanned = self.pending.len();

            if Instant::now() >= deadline {
                return (events, ReadEnd::TimedOut);
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return (events, ReadEnd::Closed),
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return (events, ReadEnd::Closed),
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

/// The section of the page under the heading carrying `{ #anchor }`, up to the next `---` line.
fn section<'a>(page: &'a str, anchor: &str) -> &'a str {
    let marker = format!("{{ #{anchor} }}");
    let at = page
        .find(&marker)
        .unwrap_or_else(|| panic!("the page has no heading {marker}"));
    let rest = &page[at..];
    &rest[..rest.find("\n---\n").unwrap_or(rest.len())]
}

/// Whether `event` is `task_id`'s terminal status.
fn is_final_status_of(event: &Event, task_id: &str) -> bool {
    event.is_terminal_status() && event.task_id() == Some(task_id)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Every `StreamResponse` member, murmur frame and frame key a real task writes is on the page,
/// on an activated `SendStreamingMessage` and on `stream/watch`, and the page's `params.frames`
/// example is a live card's.
#[test]
fn the_page_covers_every_frame_a_task_writes() {
    let server = undeclared_tool_then_end_turn();
    let capsule = start_capsule(&server, "streaming-protocol-doc");
    let addr = capsule.url();

    let (response, streamed) = stream::stream_message(
        &addr,
        None,
        json!({"message": {"messageId": "msg-1", "role": "ROLE_USER",
            "parts": [{"text": "call a tool", "mediaType": "text/plain"}]}}),
        true,
    );
    assert_eq!(
        response.header("a2a-extensions"),
        Some(stream::STREAM_EXTENSION_URI)
    );
    let task_id = streamed[0].task_id().unwrap().to_string();
    let watched = watch_until(&addr, Duration::from_secs(60), |event| {
        is_final_status_of(event, &task_id)
    });

    let page = page();
    let members = section(&page, "standard-events");
    let mut frames = BTreeSet::new();
    for event in streamed.iter().chain(&watched) {
        let member = event
            .result()
            .as_object()
            .and_then(|object| object.keys().next())
            .unwrap()
            .clone();
        assert!(
            members.contains(&format!("`{member}`")),
            "the StreamResponse member `{member}` is not in the Standard events section"
        );
        if !event.is_frame() {
            continue;
        }
        let name = event.frame()["frame"].as_str().unwrap().to_string();
        let documented = section(&page, &format!("event-{name}"));
        for key in object_keys(event.frame()) {
            assert!(
                documented.contains(&format!("`{key}`")),
                "`{name}` key `{key}` is not in its section"
            );
        }
        frames.insert(name);
    }
    for expected in ["connection-ack", "artifact", "text"] {
        assert!(
            frames.contains(expected),
            "no `{expected}` frame seen: {frames:?}"
        );
    }
    let artifact = streamed
        .iter()
        .find(|event| event.kind() == "artifact")
        .expect("the task wrote an artifact frame")
        .frame();
    assert_eq!(artifact["toolName"], UNDECLARED_TOOL);
    assert_eq!(artifact["isError"], true);
    assert_eq!(artifact["fenceSource"], Value::Null);

    let card: Value = {
        let mut stream = TcpStream::connect(&addr).unwrap();
        write!(
            stream,
            "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    };
    let live = card["capabilities"]["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|extension| extension["uri"] == stream::STREAM_EXTENSION_URI)
        .unwrap()["params"]["frames"]
        .clone();
    let example = section(&page, "murmur-stream-v1")
        .lines()
        .find_map(|line| line.trim().strip_prefix("\"params\": "))
        .map(|params| serde_json::from_str::<Value>(params.trim_end_matches(',')).unwrap())
        .expect("the stream extension section shows its params");
    assert_eq!(
        example["frames"], live,
        "the page's params.frames is not a live card's"
    );
}

#[test]
fn the_page_names_every_state_and_mode() {
    let page = page();
    for (wire, word) in [
        ("TASK_STATE_SUBMITTED", "submitted"),
        ("TASK_STATE_WORKING", "working"),
        ("TASK_STATE_INPUT_REQUIRED", "input-required"),
        ("TASK_STATE_COMPLETED", "completed"),
        ("TASK_STATE_FAILED", "failed"),
        ("TASK_STATE_CANCELED", "canceled"),
        ("TASK_STATE_REJECTED", "rejected"),
    ] {
        assert!(
            page.contains(&format!("| `{wire}` | `{word}` |")),
            "state `{wire}` has no row"
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
        let got = rpc(addr, "GetTask", json!({"id": last}));
        if got["result"]["status"]["state"] == "TASK_STATE_COMPLETED" {
            return last;
        }
        assert!(
            Instant::now() < deadline,
            "the last task never completed: {got}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The `missed` count of every `lagged` frame in `events`, asserting each is unnumbered.
fn lagged_counts(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter(|event| event.kind() == "lagged")
        .map(|event| {
            assert_eq!(event.id, None, "a lagged frame carries an id: {event:?}");
            assert_eq!(
                object_keys(event.frame()),
                vec!["frame", "missed"],
                "{event:?}"
            );
            event.frame()["missed"].as_u64().expect("missed is a u64")
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

/// A fresh `stream/watch` replays the session's buffer through `last_task`'s terminal status
/// without a `lagged` frame in it.
fn assert_replay_has_no_lagged_frame(addr: &str, last_task: &str) {
    let replay = watch_until(addr, Duration::from_secs(120), |event| {
        is_final_status_of(event, last_task)
    });
    assert!(
        replay.iter().all(|event| event.kind() != "lagged"),
        "a lagged frame was replayed"
    );
}

/// A `stream/watch` client that stops reading while a capsule writes more than 128 events is sent
/// one unnumbered `lagged` frame per loss, carrying the count the runtime reported, and keeps
/// receiving on the same connection.
#[test]
fn a_lagging_watch_connection_is_told_how_many_frames_it_lost() {
    let server = blocking_reply_then_flood();
    let capsule = start_capsule(&server, "lagged-watch");
    let addr = capsule.url();

    let mut slow = SseReader::open(connect_slow_reader(&addr), &addr, WATCH_BODY, &[]);
    let (_, end) = slow.read_until(Duration::from_secs(30), |event| {
        event.kind() == "connection-ack"
    });
    assert_eq!(end, ReadEnd::Accepted, "no connection-ack");

    let blocking_task = submit(&addr, "block", "reply at length");
    let last_task = flood(&server, &addr);

    let (events, end) = slow.read_until(Duration::from_secs(120), |event| {
        is_final_status_of(event, &last_task)
    });
    assert_eq!(
        end,
        ReadEnd::Accepted,
        "the watch did not reach the last task's final status"
    );

    let sent = lagged_counts(&events);
    assert!(!sent.is_empty(), "no lagged frame reached the connection");
    for event in events.iter().filter(|event| event.kind() == "lagged") {
        assert_eq!(event.result()["message"]["role"], "ROLE_AGENT", "{event:?}");
    }
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
        !events
            .iter()
            .any(|event| is_final_status_of(event, &blocking_task)),
        "the blocking task's final status was delivered, so nothing was lost before it"
    );

    assert_replay_has_no_lagged_frame(&addr, &last_task);
}

/// A `SendStreamingMessage` client that activated the stream extension and stops reading while a
/// capsule writes more than 128 events is sent `lagged` frames carrying the counts the runtime
/// reported, as carriers of its own task. Its task's ending was among the events lost, so once
/// the task has ended the connection writes it from the registry, unnumbered, and closes, having
/// written no other task's event.
#[test]
fn a_lagging_message_stream_connection_is_told_how_many_frames_it_lost() {
    let server = blocking_reply_then_flood();
    let capsule = start_capsule(&server, "lagged-message-stream");
    let addr = capsule.url();

    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendStreamingMessage",
        "params": {"message": {
            "messageId": "block",
            "role": "ROLE_USER",
            "parts": [{"text": "reply at length", "mediaType": "text/plain"}]
        }}
    })
    .to_string();
    let mut slow = SseReader::open(
        connect_slow_reader(&addr),
        &addr,
        &body,
        &[stream::extensions_header()],
    );
    let last_task = flood(&server, &addr);

    let (events, end) = slow.read_until(Duration::from_secs(120), |_| false);
    assert_eq!(
        end,
        ReadEnd::Closed,
        "the connection did not end: {events:#?}"
    );

    let own_task = events[0]
        .task_id()
        .expect("the stream opens with its task")
        .to_string();
    assert!(
        events
            .iter()
            .all(|event| event.task_id() == Some(own_task.as_str())),
        "another task's event reached the connection"
    );
    assert!(!events
        .iter()
        .any(|event| event.task_id() == Some(last_task.as_str())));

    let sent = lagged_counts(&events);
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

    let last = events.last().unwrap();
    assert!(is_final_status_of(last, &own_task), "{last:?}");
    assert_eq!(last.state(), Some("TASK_STATE_COMPLETED"));
    let got = rpc(&addr, "GetTask", json!({"id": own_task}));
    assert_eq!(
        last.status(),
        &got["result"]["status"],
        "the stream ends on the status GetTask reports"
    );
    assert_eq!(
        last.id, None,
        "the lost ending is written from the registry, outside the sequence"
    );

    assert_replay_has_no_lagged_frame(&addr, &last_task);
}

/// `mur watch` attached to a capsule that ran one task prints the task's lines: its statuses, the
/// failed call's artifact and the answer. Durations are normalised, since they are measured.
#[test]
fn watch_prints_the_same_lines() {
    let server = undeclared_tool_then_end_turn();
    let capsule = start_capsule(&server, "watch-prints-lines");
    let addr = capsule.url();

    let task_id = submit(&addr, "msg-1", "call a tool");
    let deadline = Instant::now() + Duration::from_secs(120);
    while rpc(&addr, "GetTask", json!({"id": task_id}))["result"]["status"]["state"]
        != "TASK_STATE_COMPLETED"
    {
        assert!(Instant::now() < deadline, "the task never completed");
        thread::sleep(Duration::from_millis(50));
    }

    let mut watch = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["watch", "--url", &addr])
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mur watch should start");
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout = watch.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while lines.last().map(String::as_str) != Some("  done") {
        let left = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(left) {
            Ok(line) => lines.push(line),
            Err(_) => break,
        }
    }
    while let Ok(line) = line_rx.recv_timeout(Duration::from_secs(1)) {
        lines.push(line);
    }
    let _ = watch.kill();
    let _ = watch.wait();

    let normalised: Vec<String> = lines
        .iter()
        .map(|line| {
            let mut out = String::new();
            let mut digits = String::new();
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c.is_ascii_digit() {
                    digits.push(c);
                    continue;
                }
                if !digits.is_empty() {
                    if c == 'm' && chars.peek() == Some(&'s') {
                        out.push('N');
                    } else {
                        out.push_str(&digits);
                    }
                    digits.clear();
                }
                out.push(c);
            }
            out.push_str(&digits);
            out
        })
        .collect();
    let transcript = normalised.join("\n");
    eprintln!("mur watch printed:\n{transcript}");
    assert_eq!(transcript, WATCH_TRANSCRIPT);
}

/// What `mur watch` prints for [`watch_prints_the_same_lines`]'s task, with every `<n>ms` written
/// `Nms`. The stream's wire form must not change these lines.
const WATCH_TRANSCRIPT: &str = "[working]  inference turn 1
[artifact] tool: no-such-tool [error, Nms] | tool 'no-such-tool' is not declared in manifest allowlist
[working]  inference turn 2
done[completed]
  done
";
