//! Integration tests for what a streaming client sees of a task its `on-task-end` hooks reopen.
//!
//! Every task that runs ends in exactly one `status` frame with `"final":true`, written after the
//! hooks have had their say and agreeing with `tasks/get` and the trace's `task_end`. Between two
//! attempts the stream carries one non-final `working` frame naming the hook, with `status.reopen`
//! set to the reopen's ordinal. `message/stream` closes on its own task's final status only.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use common::idle_capsule::{
    end_turn, end_turns, http_post_json, launch_idle_capsule,
    launch_idle_capsule_with_input_timeout, open_watch, IdleCapsule,
};
use serde_json::Value;

const REOPEN_ONCE_HOOK: &str = "reopen-once";
const REOPEN_ALWAYS_HOOK: &str = "reopen-always";
const TOOL_NAME: &str = "request-input-tool";
const TOOL_VERSION: &str = "0.1.0";

/// How long a `message/stream` is read before a test gives up on the capsule closing it.
const STREAM_DEADLINE: Duration = Duration::from_secs(60);

// ── capsules ───────────────────────────────────────────────────────────────────

/// The `artifacts:` entry and published zip for an `on-task-end` hook named `name`.
fn publish_reopen_hook(home: &tempfile::TempDir, dir: &Path, name: &str, wasm: &[u8]) -> String {
    let hook = common::hook_wat::create_hook_zip(dir, name, "on-task-end", "reopen-task", wasm);
    common::publish_local(home, &hook).success();
    format!("  - name: {name}\n    version: 0.1.0\n    runtime: hook\n")
}

fn create_tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&artifact_path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    write!(
        zip,
        "name: {TOOL_NAME}\nversion: {TOOL_VERSION}\nruntime: wasm\n"
    )
    .unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(
        &fs::read(common::fixture_path(
            "input-required/tool/request-input-tool.wasm",
        ))
        .unwrap(),
    )
    .unwrap();
    zip.finish().unwrap();
    artifact_path
}

/// A queue + sleep capsule binding `hook` (a name and its component) to `on-task-end`.
fn launch_with_hook(
    server: common::ScriptedServer,
    name: &'static str,
    wasm: Vec<u8>,
) -> IdleCapsule {
    common::idle_capsule::launch_idle_capsule_with(server, move |home, dir| {
        publish_reopen_hook(home, dir, name, &wasm)
    })
}

/// A queue + sleep capsule declaring `request-input-tool` with a 2 s `lifecycle.input_timeout_secs`,
/// and the reopen-once hook when `reopen_once` holds.
fn launch_with_input_tool(server: common::ScriptedServer, reopen_once: bool) -> IdleCapsule {
    launch_idle_capsule_with_input_timeout(server, Some(2), move |home, dir| {
        common::publish_local(home, &create_tool_artifact(dir)).success();
        let mut extra =
            format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n    runtime: tool\n");
        if reopen_once {
            extra.push_str(&publish_reopen_hook(
                home,
                dir,
                REOPEN_ONCE_HOOK,
                &common::hook_wat::reopen_task_once_hook_wasm("check again"),
            ));
        }
        extra
    })
}

/// An anthropic reply asking for `request-input-tool`.
fn request_input_call(n: usize) -> String {
    serde_json::json!({
        "id": format!("msg_{n}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": "call_1",
            "name": TOOL_NAME,
            "input": {"data": "Which option?"}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── the stream ─────────────────────────────────────────────────────────────────

/// One SSE frame as written: its `id:` (if any), its event type and its parsed data.
#[derive(Debug, Clone, PartialEq)]
struct Frame {
    id: Option<u64>,
    event: String,
    data: Value,
}

impl Frame {
    fn is_status(&self) -> bool {
        self.event == "status"
    }

    fn is_final_status(&self) -> bool {
        self.is_status() && self.data["final"] == true
    }

    fn is_boundary(&self) -> bool {
        self.is_status() && self.data["status"].get("reopen").is_some()
    }

    fn state(&self) -> &str {
        self.data["status"]["state"].as_str().unwrap_or_default()
    }

    fn message(&self) -> &str {
        self.data["status"]["message"].as_str().unwrap_or_default()
    }

    fn task_id(&self) -> &str {
        self.data["id"].as_str().unwrap_or_default()
    }
}

/// What one `message/stream` connection received.
struct Stream {
    frames: Vec<Frame>,
    /// Whether the capsule closed the connection before the deadline.
    closed: bool,
}

impl Stream {
    fn finals(&self) -> Vec<&Frame> {
        self.frames.iter().filter(|f| f.is_final_status()).collect()
    }

    fn boundaries(&self) -> Vec<&Frame> {
        self.frames.iter().filter(|f| f.is_boundary()).collect()
    }

    /// The connection's own task's final status, asserted to be the last frame on the connection
    /// and the only final status that task has.
    fn the_final(&self) -> &Frame {
        assert!(
            self.closed,
            "the capsule closed the stream: {:#?}",
            self.frames
        );
        let last = self.frames.last().expect("the stream carried frames");
        assert!(
            last.is_final_status(),
            "the final status is the last frame: {:#?}",
            self.frames
        );
        let own = self
            .finals()
            .into_iter()
            .filter(|f| f.task_id() == last.task_id())
            .count();
        assert_eq!(own, 1, "exactly one final status: {:#?}", self.frames);
        last
    }

    /// The id of the task this connection submitted: the task its final status names.
    fn own_task_id(&self) -> String {
        self.the_final().task_id().to_string()
    }

    /// Index of the first frame at or after `from` that `matches`.
    fn index(&self, from: usize, matches: impl Fn(&Frame) -> bool) -> usize {
        from + self.frames[from..]
            .iter()
            .position(matches)
            .unwrap_or_else(|| panic!("no matching frame after {from}: {:#?}", self.frames))
    }

    /// Every numbered frame's id, asserted strictly ascending.
    fn assert_ids_ascend(&self) {
        let ids: Vec<u64> = self.frames.iter().filter_map(|f| f.id).collect();
        assert!(!ids.is_empty(), "{:#?}", self.frames);
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "numbered ids ascend strictly: {ids:?}"
        );
    }
}

/// Send `message/stream` for a new task and return the connection, positioned at the first byte
/// of the HTTP response.
fn open_message_stream(addr: &str, message_id: &str, text: &str) -> TcpStream {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {
            "message": {
                "messageId": message_id,
                "role": "user",
                "parts": [{"text": text}]
            }
        }
    })
    .to_string();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
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

/// Read `conn` until the capsule closes it or [`STREAM_DEADLINE`] passes, and parse its body.
fn read_until_closed(conn: TcpStream) -> Stream {
    let deadline = Instant::now() + STREAM_DEADLINE;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let mut source = &conn;
    let mut closed = false;
    while Instant::now() < deadline {
        match source.read(&mut buf) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let body = text.split_once("\n\n").map(|(_, body)| body).unwrap_or("");
    Stream {
        frames: parse_frames(body),
        closed,
    }
}

/// Parse an SSE body into frames, dropping comments such as the heartbeat.
fn parse_frames(body: &str) -> Vec<Frame> {
    let mut out = Vec::new();
    for block in body.split("\n\n") {
        let (mut id, mut event, mut data) = (None, None, None);
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("id: ") {
                id = rest.parse().ok();
            } else if let Some(rest) = line.strip_prefix("event: ") {
                event = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("data: ") {
                data = serde_json::from_str::<Value>(rest).ok();
            }
        }
        if let (Some(event), Some(data)) = (event, data) {
            out.push(Frame { id, event, data });
        }
    }
    out
}

/// Stream one task from submission to the capsule closing the connection.
fn stream_task(capsule: &IdleCapsule, message_id: &str, text: &str) -> Stream {
    read_until_closed(open_message_stream(&capsule.url, message_id, text))
}

/// Every frame a `stream/watch` connection replays from `Last-Event-ID: 0`, read until `task_id`'s
/// final status arrives or [`STREAM_DEADLINE`] passes. The observer's `connection-ack` is dropped.
fn replay(capsule: &IdleCapsule, task_id: &str) -> Vec<Frame> {
    let conn = open_watch(&capsule.url, 0);
    let deadline = Instant::now() + STREAM_DEADLINE;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let mut source = &conn;
    loop {
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let body = text.split_once("\n\n").map(|(_, body)| body).unwrap_or("");
        let frames: Vec<Frame> = parse_frames(body)
            .into_iter()
            .filter(|f| f.event != "connection-ack")
            .collect();
        if Instant::now() >= deadline
            || frames
                .iter()
                .any(|f| f.is_final_status() && f.task_id() == task_id)
        {
            return frames;
        }
        match source.read(&mut buf) {
            Ok(0) => return frames,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return frames,
        }
    }
}

// ── tasks and trace ────────────────────────────────────────────────────────────

fn task_state(addr: &str, task_id: &str) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": task_id}
    });
    http_post_json(addr, &body.to_string())["result"]["status"]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The session trace's records of `event_type` for `task_id`.
fn trace_records(workdir: &Path, task_id: &str, event_type: &str) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["event_type"] == event_type && record["task_id"] == task_id)
        .collect()
}

/// The boundary frame for the first reopen by `hook`, checked for its exact shape.
fn assert_first_boundary(stream: &Stream, task_id: &str, hook: &str) {
    let boundaries = stream.boundaries();
    assert_eq!(
        boundaries.len(),
        1,
        "one boundary frame: {:#?}",
        stream.frames
    );
    let boundary = boundaries[0];
    assert!(boundary.id.is_some(), "the boundary frame is numbered");
    assert_eq!(boundary.data["id"], task_id);
    assert_eq!(boundary.data["final"], false);
    assert_eq!(boundary.state(), "working");
    assert_eq!(boundary.message(), format!("reopened by hook {hook}"));
    assert_eq!(boundary.data["status"]["reopen"], 1);
    assert!(boundary.data["status"].get("response").is_none());
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// A task a hook reopens once and then accepts ends in one final `completed` status carrying the
/// second attempt's answer. The first attempt's text still went out, followed by one boundary
/// frame before the second attempt's first turn, and a `stream/watch` replay shows the same frames.
#[test]
fn a_reopened_task_is_told_it_finished_once_with_the_accepted_answer() {
    let capsule = launch_with_hook(
        common::ScriptedServer::start(end_turns(&[
            "Noted, first attempt.",
            "Noted, second attempt.",
        ])),
        REOPEN_ONCE_HOOK,
        common::hook_wat::reopen_task_once_hook_wasm("check again"),
    );
    let stream = stream_task(&capsule, "reopen-stream-s1", "take a note");

    let last = stream.the_final();
    assert_eq!(last.state(), "completed");
    assert_eq!(last.data["status"]["response"], "Noted, second attempt.");
    assert!(
        !stream.frames.iter().any(|f| f.is_status()
            && f.state() == "completed"
            && f.data["status"]["response"] == "Noted, first attempt."),
        "the rejected answer is never reported completed: {:#?}",
        stream.frames
    );
    let task_id = stream.own_task_id();
    assert_first_boundary(&stream, &task_id, REOPEN_ONCE_HOOK);

    let first_text = stream.index(0, |f| {
        f.event == "text" && f.data["text"] == "Noted, first attempt."
    });
    let boundary = stream.index(0, Frame::is_boundary);
    // The second attempt's first turn numbers on from the first attempt's one turn.
    let second_turn = stream.index(boundary, |f| {
        f.is_status() && f.message() == "inference turn 2"
    });
    assert!(
        first_text < boundary && boundary < second_turn,
        "{:#?}",
        stream.frames
    );
    stream.assert_ids_ascend();
    assert_eq!(
        replay(&capsule, &task_id),
        stream.frames,
        "a stream/watch replay shows the live stream's frames, boundary and final status included"
    );

    assert_eq!(task_state(&capsule.url, &task_id), "completed");
    assert_eq!(
        trace_records(&capsule.workdir, &task_id, "task_reopened").len(),
        1
    );
    let end = trace_records(&capsule.workdir, &task_id, "task_end");
    assert_eq!(end.len(), 1);
    assert_eq!(end[0]["exit_status"], "ok");
    assert_eq!(end[0]["reopen_count"], 1);
}

/// A hook that still wants a reopen when `lifecycle.max_task_reopens` is spent leaves the task
/// one final `failed` status naming that limit, and it is never reported `completed`.
#[test]
fn a_task_refused_a_reopen_is_told_it_failed_once() {
    let capsule = launch_with_hook(
        common::ScriptedServer::start(end_turns(&["Attempt one.", "Attempt two."])),
        REOPEN_ALWAYS_HOOK,
        common::hook_wat::reopen_task_hook_wasm("never good enough"),
    );
    let stream = stream_task(&capsule, "reopen-stream-s2", "try it");

    let last = stream.the_final();
    assert_eq!(last.state(), "failed");
    assert!(
        last.message().contains("lifecycle.max_task_reopens"),
        "{}",
        last.message()
    );
    assert!(
        !stream
            .frames
            .iter()
            .any(|f| f.is_status() && f.state() == "completed"),
        "{:#?}",
        stream.frames
    );
    let task_id = stream.own_task_id();
    assert_first_boundary(&stream, &task_id, REOPEN_ALWAYS_HOOK);

    assert_eq!(task_state(&capsule.url, &task_id), "failed");
    let end = trace_records(&capsule.workdir, &task_id, "task_end");
    assert_eq!(end.len(), 1);
    assert_eq!(end[0]["exit_status"], "reopen_budget_exhausted");
}

/// An attempt that failed and was reopened writes no `failed` status; the task's one final
/// status is the reopened attempt's `completed`.
#[test]
fn a_failed_attempt_that_is_reopened_is_not_reported_failed() {
    let capsule = launch_with_hook(
        common::ScriptedServer::start(vec![
            "this is not json".to_string(),
            end_turn(2, "Recovered."),
        ]),
        REOPEN_ONCE_HOOK,
        common::hook_wat::reopen_task_once_hook_wasm("check again"),
    );
    let stream = stream_task(&capsule, "reopen-stream-s3", "recover");

    let last = stream.the_final();
    assert_eq!(last.state(), "completed");
    assert_eq!(last.data["status"]["response"], "Recovered.");
    assert!(
        !stream
            .frames
            .iter()
            .any(|f| f.is_status() && f.state() == "failed"),
        "{:#?}",
        stream.frames
    );
    let task_id = stream.own_task_id();
    assert_first_boundary(&stream, &task_id, REOPEN_ONCE_HOOK);

    assert_eq!(task_state(&capsule.url, &task_id), "completed");
    assert_eq!(
        trace_records(&capsule.workdir, &task_id, "task_failed").len(),
        1,
        "the first attempt's failure is recorded"
    );
    assert_eq!(
        trace_records(&capsule.workdir, &task_id, "task_reopened").len(),
        1
    );
}

/// A `request-input` wait that times out with no hook bound ends the task in one final
/// `failed` status with message `input-timeout`, and nothing reports it failed earlier.
#[test]
fn an_input_timeout_ends_the_task_with_one_failed_status() {
    let capsule = launch_with_input_tool(
        common::ScriptedServer::start(vec![request_input_call(1), end_turn(2, "Too late.")]),
        false,
    );
    let stream = stream_task(&capsule, "reopen-stream-s5a", "ask me something");

    let asked = stream.index(0, |f| f.is_status() && f.state() == "input-required");
    let last = stream.the_final();
    assert_eq!(last.state(), "failed");
    assert_eq!(last.message(), "input-timeout");
    assert_eq!(
        stream
            .frames
            .iter()
            .filter(|f| f.is_status() && f.state() == "failed")
            .count(),
        1,
        "{:#?}",
        stream.frames
    );
    assert!(asked < stream.frames.len() - 1);
    assert!(stream.boundaries().is_empty(), "{:#?}", stream.frames);
    assert_eq!(task_state(&capsule.url, &stream.own_task_id()), "failed");
}

/// The same timeout on a task a hook reopens is not the task's end: `tasks/get` never reads
/// `failed`, no `failed` frame is written, and the reopened attempt's answer is the one final
/// `completed` status.
#[test]
fn an_input_timeout_a_hook_reopens_is_not_reported_failed() {
    let capsule = launch_with_input_tool(
        common::ScriptedServer::start(vec![request_input_call(1), end_turn(2, "Answered anyway.")]),
        true,
    );

    // Every state `tasks/get` answers for the active task while the stream is open.
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let poller = {
        let (seen, stop, url) = (Arc::clone(&seen), Arc::clone(&stop), capsule.url.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let body = r#"{"jsonrpc":"2.0","id":1,"method":"tasks/get","params":{}}"#;
                let response = http_post_json(&url, body);
                if let Some(state) = response["result"]["status"]["state"].as_str() {
                    seen.lock().unwrap().push(state.to_string());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let stream = stream_task(&capsule, "reopen-stream-s5b", "ask me something");
    stop.store(true, Ordering::Relaxed);
    poller.join().unwrap();

    let last = stream.the_final();
    assert_eq!(last.state(), "completed");
    assert_eq!(last.data["status"]["response"], "Answered anyway.");
    assert!(
        !stream
            .frames
            .iter()
            .any(|f| f.is_status() && f.state() == "failed"),
        "{:#?}",
        stream.frames
    );
    let task_id = stream.own_task_id();
    assert_first_boundary(&stream, &task_id, REOPEN_ONCE_HOOK);

    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().any(|state| state == "input-required"),
        "the poller saw the wait: {seen:?}"
    );
    assert!(
        !seen.iter().any(|state| state == "failed"),
        "tasks/get never reads failed: {seen:?}"
    );
    assert_eq!(task_state(&capsule.url, &task_id), "completed");
    assert_eq!(
        trace_records(&capsule.workdir, &task_id, "task_failed")[0]["cause"],
        "input_timeout"
    );
}

/// `message/stream` forwards other tasks' frames but closes on its own task's final status.
/// B, opened while A runs, receives A's final status and stays open until its own.
#[test]
fn message_stream_closes_on_its_own_tasks_final_status() {
    let capsule = launch_idle_capsule(common::ScriptedServer::start_with_delay(
        end_turns(&["A done.", "B done."]),
        Duration::from_secs(2),
    ));
    let a = open_message_stream(&capsule.url, "reopen-stream-s6-a", "task A");
    // A is running, waiting on its 2 s provider reply, when B is submitted.
    let deadline = Instant::now() + Duration::from_secs(30);
    while trace_count(&capsule.workdir, "task_start") < 1 {
        assert!(Instant::now() < deadline, "task A never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let b = open_message_stream(&capsule.url, "reopen-stream-s6-b", "task B");
    let b_reader = std::thread::spawn(move || read_until_closed(b));
    let a = read_until_closed(a);
    let b = b_reader.join().unwrap();

    let a_final = a.the_final();
    assert_eq!(a_final.state(), "completed");
    assert_eq!(a_final.data["status"]["response"], "A done.");

    let b_final = b.the_final();
    assert_eq!(b_final.state(), "completed");
    assert_eq!(b_final.data["status"]["response"], "B done.");
    assert_ne!(b_final.task_id(), a_final.task_id());
    assert!(
        b.frames.iter().any(|f| f.is_final_status()
            && f.task_id() == a_final.task_id()
            && f.data["status"]["response"] == "A done."),
        "B received A's final status and stayed open: {:#?}",
        b.frames
    );
    assert!(
        !a.frames.iter().any(|f| f.task_id() == b_final.task_id()),
        "A closed before B ran: {:#?}",
        a.frames
    );
}

/// How many records of `event_type` the session trace holds.
fn trace_count(workdir: &Path, event_type: &str) -> usize {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["event_type"] == event_type)
        .count()
}
