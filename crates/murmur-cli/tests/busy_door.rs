//! A capsule answers its door while its agent loop is busy.
//!
//! The busy turn is a blocking `on-inference` hook that spins until its epoch deadline traps it:
//! guest code under `call_async` with no async yield, so the thread running the agent loop is held
//! for the whole deadline. Whether the spin has begun is read off the scripted provider (the hook
//! fires on the response to its one request), and whether it is still going off the trace (the
//! turn's `inference` line is written only once the hook returns).
//!
//! Every capsule is its own `mur run` process, so `mur ps` verifies a real pid and start time and
//! `mur stop` signals a process this case owns. Each case gets its own scratch `HOME`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use common::hook_wat::{create_hook_zip, spin_hook_wasm};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// How long the hook spins, as `capabilities.limits.deadline_seconds`. Long enough that every
/// probe in a case lands inside one spin with room to spare on a loaded runner.
const SPIN_SECONDS: u64 = 20;

/// The budget each probe gets, connect and read together: `mur ps`'s connect timeout, and well
/// under its read timeout, so a pass here is a pass there with margin.
const PROBE_BUDGET: Duration = Duration::from_secs(2);

// ── The capsule ───────────────────────────────────────────────────────────────

fn end_turn_response(id: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A scratch `$HOME` with the fixture driver and the spinning `on-inference` hook published.
fn spinner_home() -> TempDir {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let hook = create_hook_zip(
        artifacts.path(),
        "spinner",
        "on-inference",
        "none",
        &spin_hook_wasm("on-inference"),
    );
    common::publish_local(&home, &hook).success();
    home
}

/// A queue + sleep capsule running the spinner, with `exports` spliced in when given.
fn spinner_project(endpoint: &str, exports: &str) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: busy-door\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             \x20   gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n  \
             - name: spinner\n    version: 0.1.0\n    runtime: hook\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n  \
             limits:\n    deadline_seconds: {SPIN_SECONDS}\n\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n\
             inference:\n  transport: http\n  model: test-model\n  \
             driver:\n    artifact: {DRIVER_NAME}\n{exports}"
        ),
    )
    .unwrap();
    project
}

struct Capsule {
    child: Child,
    project: TempDir,
    home: TempDir,
    server: common::ScriptedServer,
    /// The `mur run --json` startup line.
    startup: Value,
}

impl Capsule {
    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn trace_path(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap()).join("trace.jsonl")
    }

    fn record_path(&self) -> PathBuf {
        self.home
            .path()
            .join(".murmur")
            .join("running")
            .join(format!("{}.json", self.session_id()))
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY");
        command
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `mur run` on a spinner capsule and blocks until it has printed where its door is.
fn start_spinner(exports: &str) -> Capsule {
    let server =
        common::ScriptedServer::start(vec![end_turn_response("msg_1"), end_turn_response("msg_2")]);
    let home = spinner_home();
    let project = spinner_project(&server.endpoint, exports);
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["run", "--manifest"])
        .arg(project.path().join("murmur.yaml"))
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
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[capsule] {line}");
        }
    });

    let startup = match startup_rx.recv_timeout(Duration::from_secs(120)) {
        Ok(startup) => startup,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for the capsule to print where its door is");
        }
    };
    Capsule {
        child,
        project,
        home,
        server,
        startup,
    }
}

// ── The spin ──────────────────────────────────────────────────────────────────

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Submits one task and returns once the provider has answered its request, which is the moment
/// the `on-inference` hook is called. Returns the task id.
fn start_spin(capsule: &Capsule) -> String {
    let response = post_json(
        &capsule.url(),
        &message_send("spin-1", "spin"),
        Duration::from_secs(30),
    )
    .unwrap_or_else(|e| panic!("submitting the spinning task failed: {e}"));
    let task_id = response["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the door did not accept the task: {response}"))
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(60);
    while capsule.server.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the capsule never asked the provider for its turn"
        );
        thread::sleep(Duration::from_millis(20));
    }
    task_id
}

/// Whether the turn's hook is still spinning: the `inference` line follows the hook's return.
fn still_spinning(capsule: &Capsule) -> bool {
    !read_trace(&capsule.trace_path())
        .iter()
        .any(|event| event["event_type"] == "inference")
}

/// Waits for the spin to end and returns when the trace says it did.
fn spin_ended_at_ms(capsule: &Capsule) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(SPIN_SECONDS + 60);
    loop {
        if let Some(event) = read_trace(&capsule.trace_path())
            .into_iter()
            .find(|event| event["event_type"] == "inference")
        {
            return event["timestamp"].as_u64().expect("a timestamp");
        }
        assert!(
            Instant::now() < deadline,
            "the spin never ended; the hook's deadline did not fire"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

// ── Talking to the door, against a budget ─────────────────────────────────────

fn message_send(message_id: &str, text: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {"message": {
            "messageId": message_id,
            "role": "user",
            "parts": [{"text": text}]
        }}
    })
    .to_string()
}

/// Connects to `addr` (`localhost:<port>`, as a session announces it) within `budget`, trying
/// each address the name resolves to.
fn connect(addr: &str, budget: Duration) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::other(format!("{addr} resolved to nothing"));
    for socket in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&socket, budget) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Sends `request` and reads the response until the peer closes or `budget` runs out, whichever is
/// first. The budget covers the connect as well as the read.
fn exchange(addr: &str, request: &str, budget: Duration) -> Result<String, String> {
    let started = Instant::now();
    let stream = connect(addr, budget)
        .map_err(|e| format!("connect failed after {:?}: {e}", started.elapsed()))?;
    (&stream)
        .write_all(request.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "no complete answer within {budget:?}; read so far: {:?}",
                String::from_utf8_lossy(&response)
            ));
        }
        stream.set_read_timeout(Some(remaining)).unwrap();
        match (&stream).read(&mut buf) {
            Ok(0) => return Ok(String::from_utf8_lossy(&response).to_string()),
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}

fn status_and_body(response: &str) -> (u16, &str) {
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    (status, body)
}

fn get(addr: &str, path: &str, budget: Duration) -> Result<(u16, String), String> {
    let response = exchange(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
        budget,
    )?;
    let (status, body) = status_and_body(&response);
    Ok((status, body.to_string()))
}

fn post_json(addr: &str, body: &str, budget: Duration) -> Result<Value, String> {
    let response = exchange(
        addr,
        &format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
        budget,
    )?;
    let (_, body) = status_and_body(&response);
    serde_json::from_str(body).map_err(|e| format!("unparseable answer ({e}): {response:?}"))
}

/// Opens `stream/watch` and returns the socket and the moment it was opened.
fn open_watch(addr: &str) -> (TcpStream, Instant) {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"stream/watch","params":{}}"#;
    let opened = Instant::now();
    let stream = TcpStream::connect(addr).expect("should connect");
    (&stream)
        .write_all(
            format!(
                "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    (stream, opened)
}

/// Reads until `line` arrives, or `budget` from `since` runs out.
fn read_until_line(
    stream: &TcpStream,
    line: &str,
    since: Instant,
    budget: Duration,
) -> Result<(), String> {
    let mut reader: &TcpStream = stream;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if String::from_utf8_lossy(&seen)
            .lines()
            .any(|l| l.trim_end() == line)
        {
            return Ok(());
        }
        let remaining = budget.saturating_sub(since.elapsed());
        if remaining.is_zero() {
            return Err(format!(
                "no `{line}` within {budget:?}; read: {:?}",
                String::from_utf8_lossy(&seen)
            ));
        }
        stream.set_read_timeout(Some(remaining)).unwrap();
        match reader.read(&mut buf) {
            Ok(0) => return Err("the stream closed".to_string()),
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
}

/// One timed agent-card probe that must answer inside [`PROBE_BUDGET`] for this session.
fn probe_card(capsule: &Capsule, label: &str) -> Duration {
    let started = Instant::now();
    let (status, body) = get(&capsule.url(), "/.well-known/agent-card.json", PROBE_BUDGET)
        .unwrap_or_else(|e| panic!("{label}: the agent card went unanswered mid-turn: {e}"));
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "{label}: {body}");
    let card: Value = serde_json::from_str(&body).expect("the card is JSON");
    assert_eq!(card["session_id"], capsule.session_id().as_str(), "{label}");
    assert!(elapsed < PROBE_BUDGET, "{label}: answered in {elapsed:?}");
    elapsed
}

// ── Scenarios ─────────────────────────────────────────────────────────────────

/// While a hook holds the agent loop's thread, the agent card answers inside two seconds every
/// time it is asked, and `mur ps` lists the capsule as `running` without pruning anything.
#[test]
fn a_spinning_hook_leaves_the_card_answering_and_ps_running() {
    let capsule = start_spinner("");
    start_spin(&capsule);
    let spin_seen = Instant::now();

    let mut last_answer_ms = 0;
    for (index, offset) in [1u64, 4, 7].into_iter().enumerate() {
        let at = spin_seen + Duration::from_secs(offset);
        thread::sleep(at.saturating_duration_since(Instant::now()));
        let elapsed = probe_card(&capsule, &format!("probe {} at +{offset}s", index + 1));
        last_answer_ms = now_ms();
        eprintln!("[busy-door] card probe at +{offset}s answered in {elapsed:?}");
    }
    assert!(
        still_spinning(&capsule),
        "the spin ended before the last card probe, so the probes proved nothing"
    );

    let ps = capsule.mur().arg("ps").output().expect("mur ps runs");
    let stdout = String::from_utf8_lossy(&ps.stdout).to_string();
    let stderr = String::from_utf8_lossy(&ps.stderr).to_string();
    last_answer_ms = last_answer_ms.max(now_ms());
    assert!(ps.status.success(), "mur ps failed: {stdout}{stderr}");
    let row = stdout
        .lines()
        .find(|line| line.contains(&capsule.session_id()))
        .unwrap_or_else(|| panic!("no row for the busy capsule:\n{stdout}\n{stderr}"));
    assert_eq!(
        row.split_whitespace().nth(2),
        Some("running"),
        "a busy capsule must read as running: {row}"
    );
    assert!(
        !stderr.lines().any(|line| line.starts_with("pruned:")),
        "{stderr}"
    );
    assert!(
        still_spinning(&capsule),
        "the spin ended before `mur ps` returned, so its answer proved nothing"
    );

    let ended_ms = spin_ended_at_ms(&capsule);
    assert!(
        ended_ms >= last_answer_ms,
        "the spin ended at {ended_ms} ms, before the last answer at {last_answer_ms} ms"
    );
}

/// Every JSON-RPC door path answers mid-turn, not just the card: a status read, a second task, and
/// an observer's stream.
#[test]
fn every_door_path_answers_while_the_turn_is_busy() {
    let capsule = start_spinner("");
    let task_id = start_spin(&capsule);
    thread::sleep(Duration::from_secs(1));

    let started = Instant::now();
    let got = post_json(
        &capsule.url(),
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}})
            .to_string(),
        PROBE_BUDGET,
    )
    .unwrap_or_else(|e| panic!("tasks/get went unanswered mid-turn: {e}"));
    eprintln!("[busy-door] tasks/get answered in {:?}", started.elapsed());
    assert_eq!(got["result"]["status"]["state"], "working", "{got}");

    let started = Instant::now();
    let sent = post_json(
        &capsule.url(),
        &message_send("spin-2", "a second task"),
        PROBE_BUDGET,
    )
    .unwrap_or_else(|e| panic!("message/send went unanswered mid-turn: {e}"));
    eprintln!(
        "[busy-door] message/send answered in {:?}",
        started.elapsed()
    );
    assert_eq!(sent["result"]["status"]["state"], "submitted", "{sent}");

    let (watch, opened) = open_watch(&capsule.url());
    read_until_line(&watch, "event: connection-ack", opened, PROBE_BUDGET)
        .unwrap_or_else(|e| panic!("stream/watch went unanswered mid-turn: {e}"));
    eprintln!("[busy-door] stream/watch acked in {:?}", opened.elapsed());
    let last_answer_ms = now_ms();

    assert!(
        still_spinning(&capsule),
        "the spin ended before the last door answer, so the answers proved nothing"
    );
    assert!(spin_ended_at_ms(&capsule) >= last_answer_ms);
}

/// The resource plane is served by the same door, off the same thread.
#[test]
fn a_declared_export_answers_while_the_turn_is_busy() {
    if common::skip_without_host_support("a_declared_export_answers_while_the_turn_is_busy") {
        return;
    }
    let capsule = start_spinner("exports:\n  files:\n    root: out/\n    mode: read-only\n");
    let out = capsule.project.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("report.md"), b"# report\n").unwrap();

    start_spin(&capsule);
    thread::sleep(Duration::from_secs(1));

    let started = Instant::now();
    let (status, body) = get(&capsule.url(), "/resources/files", PROBE_BUDGET)
        .unwrap_or_else(|e| panic!("the resource plane went unanswered mid-turn: {e}"));
    eprintln!(
        "[busy-door] /resources/files answered in {:?}",
        started.elapsed()
    );
    assert_eq!(status, 200, "{body}");
    let last_answer_ms = now_ms();

    assert!(still_spinning(&capsule));
    assert!(spin_ended_at_ms(&capsule) >= last_answer_ms);
}

/// The door's connections run on the runtime's workers rather than on the session's own task set,
/// so nothing the session tears down reaches them by default. An observer connected through the
/// end of the session still sees the door close with it: the process exits, the observer reads
/// EOF, a fresh connect is refused, and the record is gone.
#[test]
fn an_open_observer_does_not_outlive_the_session() {
    let mut capsule = start_spinner("");
    let url = capsule.url();

    let (watch, opened) = open_watch(&url);
    read_until_line(
        &watch,
        "event: connection-ack",
        opened,
        Duration::from_secs(10),
    )
    .unwrap_or_else(|e| panic!("stream/watch did not open: {e}"));

    capsule.mur().args(["stop", "@1"]).assert().success();

    let deadline = Instant::now() + Duration::from_secs(10);
    while capsule.child.try_wait().expect("waitable").is_none() {
        assert!(
            Instant::now() < deadline,
            "the capsule was still running 10 seconds after the stop"
        );
        thread::sleep(Duration::from_millis(50));
    }

    watch
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut rest = Vec::new();
    match (&watch).read_to_end(&mut rest) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!(
            "the observer did not reach EOF: {e}; read {:?}",
            String::from_utf8_lossy(&rest)
        ),
    }

    assert!(
        connect(&url, Duration::from_secs(2)).is_err(),
        "the door still accepts connections after the session ended"
    );
    assert!(
        !capsule.record_path().exists(),
        "the running record outlived the session"
    );
}
