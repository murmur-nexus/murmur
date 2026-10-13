//! A `SendStreamingMessage` stream is an A2A v1.0 stream, end to end on an in-process door: it
//! opens with the task, reports every status change as a `statusUpdate`, delivers a completed
//! task's answer as the `response` `artifactUpdate`, and ends after the task's terminal status or
//! once the task stops for input. Only its own task's events are written, and murmur's frames only
//! to a client whose `A2A-Extensions` header names the stream extension, carried in `statusUpdate`
//! metadata so every event still parses as a v1.0 `StreamResponse`.
//!
//! Every stream is read through `common::stream`, so each event is also checked against the A2A
//! v1.0 proto by the wire recorder.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use common::door_capsule::{
    agent_project, driver_home, end_turn, launch_door, message, request, rpc, stage_door,
    stage_door_with_tools, Response, A2A_VERSION, DRIVER_NAME, DRIVER_VERSION, QUEUE_SLEEP_YAML,
};
use common::stream::{
    extensions_header, of_task, stream_message, stream_message_with, Event, Watch,
    STREAM_EXTENSION_URI,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The tool the scripted provider asks for. The capsule declares no tools, so the call fails at
/// dispatch.
const UNDECLARED_TOOL: &str = "no-such-tool";

/// An in-process public door and what must outlive it.
struct Door {
    addr: String,
    _provider: Option<common::ScriptedServer>,
    _home: TempDir,
    _project: TempDir,
}

impl Door {
    /// A door on the anthropic fixture driver against `provider`, with `lifecycle` as its
    /// lifecycle block.
    fn anthropic(name: &str, provider: common::ScriptedServer, lifecycle: &str) -> Self {
        let home = driver_home();
        let project = agent_project(&provider.endpoint, name, "", lifecycle);
        let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
        Door {
            addr: launch_door(staged),
            _provider: Some(provider),
            _home: home,
            _project: project,
        }
    }

    /// A queue+sleep door whose fixture driver streams a thought and some text, starts the echo
    /// tool's call and reports its size, then answers `PROGRESS-ANSWER`. Needs no provider.
    fn tool_progress(name: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let driver = common::create_driver_artifact(
            artifacts.path(),
            "tool-progress-driver",
            "0.1.0",
            &common::fixture_path("tool-progress-driver/tool/tool-progress-driver.wasm"),
        );
        common::publish_local(&home, &driver).success();
        let tool = common::create_tool_artifact(
            artifacts.path(),
            "echo-tool",
            "0.1.0",
            &common::fixture_path("run/components/echo-tool.wasm"),
        );
        common::publish_local(&home, &tool).success();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("murmur.yaml"),
            format!(
                "name: {name}\nversion: 0.1.0\n\
                 artifacts:\n  - name: tool-progress-driver\n    version: 0.1.0\n    \
                 runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:1\n      \
                 api_key: test-key\n  - name: echo-tool\n    version: 0.1.0\n    runtime: tool\n\
                 inference:\n  transport: http\n  model: tool-progress-streaming\n  driver:\n    \
                 artifact: tool-progress-driver\n{QUEUE_SLEEP_YAML}"
            ),
        )
        .unwrap();
        let staged = stage_door_with_tools(&home, &project.path().join("murmur.yaml"));
        Door {
            addr: launch_door(staged),
            _provider: None,
            _home: home,
            _project: project,
        }
    }

    /// A queue+sleep door whose model may call the `request-input-tool` fixture, against
    /// `provider`.
    fn request_input(provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let artifacts = tempfile::tempdir().unwrap();
        common::publish_local(&home, &request_input_tool(artifacts.path())).success();
        let endpoint = &provider.endpoint;
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("murmur.yaml"),
            format!(
                "name: waiting-agent\nversion: 0.1.0\n\
                 artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    \
                 runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n  - name: {REQUEST_INPUT_TOOL}\n    version: 0.1.0\n    \
                 runtime: tool\n\
                 capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    \
                 artifact: {DRIVER_NAME}\n{QUEUE_SLEEP_YAML}"
            ),
        )
        .unwrap();
        let staged = stage_door_with_tools(&home, &project.path().join("murmur.yaml"));
        Door {
            addr: launch_door(staged),
            _provider: Some(provider),
            _home: home,
            _project: project,
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let response = rpc(&self.addr, None, method, params);
        assert_eq!(response.status, 200, "{response:?}");
        response.json()
    }

    fn get_task(&self, task_id: &str) -> Value {
        self.rpc("GetTask", json!({"id": task_id}))["result"].clone()
    }

    /// A `SendStreamingMessage` with `params`, read to its end.
    fn stream(&self, params: Value, extensions: bool) -> (Response, Vec<Event>) {
        stream_message(&self.addr, None, params, extensions)
    }
}

fn tool_call_response(n: usize, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": format!("msg_{n}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One call to the undeclared tool, then the answer `four`.
fn tool_then_four() -> common::ScriptedServer {
    common::ScriptedServer::start(vec![
        tool_call_response(1, "toolu_1", UNDECLARED_TOOL, json!({})),
        end_turn(2, "four"),
    ])
}

const REQUEST_INPUT_TOOL: &str = "request-input-tool";

/// The `request-input-tool` fixture packed as an artifact in `dir`.
fn request_input_tool(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{REQUEST_INPUT_TOOL}-0.1.0.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(
        zip,
        "name: {REQUEST_INPUT_TOOL}\nversion: 0.1.0\nruntime: wasm"
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
    path
}

/// The provider's answer asking the `request-input-tool` for input with `prompt`.
fn ask_for_input(n: usize, prompt: &str) -> String {
    tool_call_response(
        n,
        &format!("call_{n}"),
        REQUEST_INPUT_TOOL,
        json!({"data": prompt}),
    )
}

/// A message naming the task `task_id`, replying `text`.
fn reply(task_id: &str, text: &str) -> Value {
    json!({"message": {"messageId": format!("reply-{text}"), "role": "ROLE_USER",
        "taskId": task_id, "parts": [{"text": text, "mediaType": "text/plain"}]}})
}

/// The kind, state and artifact id of each event that carries no murmur frame.
fn standard_shape(events: &[Event]) -> Vec<(String, Option<String>, Option<String>)> {
    events
        .iter()
        .filter(|event| !event.is_frame())
        .map(|event| {
            (
                event.kind(),
                event.state().map(str::to_string),
                event
                    .artifact()
                    .and_then(|artifact| artifact["artifactId"].as_str())
                    .map(str::to_string),
            )
        })
        .collect()
}

/// The ids of the numbered events, which strictly ascend.
fn assert_ids_ascend(events: &[Event]) {
    let ids: Vec<u64> = events.iter().filter_map(|event| event.id).collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids do not ascend: {ids:?}"
    );
}

/// What every `SendStreamingMessage` stream of one task holds, activated or not: it opens with the
/// unnumbered task, carries only that task's events, and its last event is the terminal status
/// `GetTask` reports afterwards.
fn assert_one_task_stream(door: &Door, events: &[Event], opening_state: &str) -> String {
    let opening = &events[0];
    assert_eq!(opening.kind(), "task", "{opening:?}");
    assert_eq!(opening.id, None, "the opening task is unnumbered");
    assert_eq!(opening.state(), Some(opening_state));
    let task_id = opening.task_id().unwrap().to_string();
    assert!(
        events
            .iter()
            .all(|event| event.task_id() == Some(task_id.as_str())),
        "another task's event: {events:#?}"
    );
    assert_ids_ascend(events);
    let last = events.last().unwrap();
    assert!(last.is_terminal_status(), "{last:?}");
    assert_eq!(
        last.status(),
        &door.get_task(&task_id)["status"],
        "the terminal status is GetTask's"
    );
    task_id
}

/// With no `A2A-Extensions`, the stream is pure v1.0 — the task, its statuses, the `response`
/// artifact and the terminal status — answering the request's id verbatim, string or integer.
#[test]
fn plain_stream_is_pure_v1_and_ends_after_the_terminal_status() {
    for (index, request_id) in [json!("req-7"), json!(9)].into_iter().enumerate() {
        let door = Door::anthropic(
            &format!("plain-stream-{index}"),
            tool_then_four(),
            QUEUE_SLEEP_YAML,
        );
        let started = Instant::now();
        let (response, events) = stream_message_with(
            &door.addr,
            None,
            request_id.clone(),
            message("m-plain", "what is 2 + 2"),
            &[],
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the stream ended {:?} after it opened",
            started.elapsed()
        );
        assert_eq!(response.status, 200);
        assert_eq!(response.header("a2a-extensions"), None, "{response:?}");
        assert!(
            !response.body.contains("event:"),
            "no event has an event: line: {}",
            response.body
        );
        assert!(
            events.iter().all(|event| event.json["id"] == request_id),
            "{events:#?}"
        );

        let task_id = assert_one_task_stream(&door, &events, "TASK_STATE_SUBMITTED");
        assert!(
            events.iter().any(|event| event.kind() == "status-update"
                && event.murmur_state() == Some("working")
                && event.status_text() == Some("inference turn 1")),
            "{events:#?}"
        );
        let artifacts: Vec<&Event> = events
            .iter()
            .filter(|event| event.kind() == "artifact-update")
            .collect();
        assert_eq!(artifacts.len(), 1, "{events:#?}");
        assert_eq!(artifacts[0].artifact().unwrap()["artifactId"], "response");
        assert_eq!(artifacts[0].artifact_text(), Some("four"));
        assert_eq!(artifacts[0].result()["artifactUpdate"]["lastChunk"], true);
        let last = events.last().unwrap();
        assert_eq!(last.state(), Some("TASK_STATE_COMPLETED"));
        assert_eq!(
            events[events.len() - 2].kind(),
            "artifact-update",
            "the answer immediately precedes COMPLETED"
        );
        assert!(events.iter().all(|event| !event.is_frame()), "{events:#?}");
        for absent in [STREAM_EXTENSION_URI, UNDECLARED_TOOL, "toolu_1"] {
            assert!(
                !response.body.contains(absent),
                "the plain stream mentions {absent}: {}",
                response.body
            );
        }
        assert!(door.get_task(&task_id)["artifacts"].is_array());
    }
}

/// An activated stream carries the same standard events as a plain one for the same work,
/// plus murmur's frames as `WORKING` carriers of the task, and echoes the extension.
#[test]
fn activated_stream_adds_murmur_frames_and_echoes_the_extension() {
    let plain_door = Door::tool_progress("activated-plain");
    let (plain_response, plain) = plain_door.stream(message("m-plain", "go"), false);
    assert_eq!(plain_response.header("a2a-extensions"), None);
    assert_one_task_stream(&plain_door, &plain, "TASK_STATE_SUBMITTED");
    assert!(plain.iter().all(|event| !event.is_frame()), "{plain:#?}");

    let door = Door::tool_progress("activated");
    let (response, events) = door.stream(message("m-activated", "go"), true);
    assert_eq!(
        response.header("a2a-extensions"),
        Some(STREAM_EXTENSION_URI),
        "{response:?}"
    );
    assert_one_task_stream(&door, &events, "TASK_STATE_SUBMITTED");
    assert_eq!(standard_shape(&events), standard_shape(&plain));

    let frames: Vec<&Value> = events
        .iter()
        .filter(|event| event.is_frame())
        .map(Event::frame)
        .collect();
    for (frame, check) in [
        ("text", Some(json!(false))),
        ("text", Some(json!(true))),
        ("thinking", None),
        ("tool-call-started", None),
        ("tool-call-progress", None),
        ("artifact", None),
    ] {
        assert!(
            frames.iter().any(|carried| carried["frame"] == frame
                && check.as_ref().is_none_or(|last| &carried["final"] == last)),
            "no {frame} {check:?} carrier: {frames:#?}"
        );
    }

    let first_working = events
        .iter()
        .position(|event| {
            !event.is_frame()
                && event.kind() == "status-update"
                && event.murmur_state() == Some("working")
        })
        .expect("a working status");
    for (index, event) in events.iter().enumerate().filter(|(_, e)| e.is_frame()) {
        assert!(
            index > first_working,
            "a carrier precedes WORKING: {event:?}"
        );
        assert_eq!(event.kind(), event.frame()["frame"].as_str().unwrap());
        let status = &event.result()["statusUpdate"]["status"];
        let timestamp = status["timestamp"].as_str().unwrap_or_default();
        assert!(
            timestamp.ends_with('Z') && chrono::DateTime::parse_from_rfc3339(timestamp).is_ok(),
            "a carrier is stamped as it is written: {event:?}"
        );
        assert_eq!(
            *status,
            json!({"state": "TASK_STATE_WORKING", "timestamp": timestamp}),
            "{event:?}"
        );
        assert!(event.id.is_some(), "a task's frame is numbered: {event:?}");
    }
    let artifact = frames
        .iter()
        .find(|frame| frame["frame"] == "artifact")
        .unwrap();
    let keys: Vec<&str> = artifact
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected = vec![
        "frame",
        "toolName",
        "content",
        "fenceSource",
        "toolCallId",
        "isError",
        "durationMs",
        "exitCode",
        "truncated",
    ];
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        sorted, expected,
        "every artifact key is present: {artifact}"
    );
}

/// `A2A-Extensions` activates the stream extension when one comma-separated entry of any line
/// equals its URI exactly, and the response echoes only that URI. Unary methods ignore it.
#[test]
fn activated_by_an_exact_entry_on_any_a2a_extensions_line() {
    let door = Door::anthropic(
        "activation",
        common::ScriptedServer::start((1..=6).map(|n| end_turn(n, "ok")).collect()),
        QUEUE_SLEEP_YAML,
    );
    let other = "https://example.com/x/v1";
    let listed = format!("{other}, {STREAM_EXTENSION_URI}");
    let trailing = format!("{STREAM_EXTENSION_URI}/");
    let extensions = capsule_runtime::A2A_EXTENSIONS_HEADER;
    let cases: [(&[(&str, &str)], bool); 4] = [
        (&[(extensions, &listed)], true),
        (
            &[(extensions, other), (extensions, STREAM_EXTENSION_URI)],
            true,
        ),
        (&[(extensions, other)], false),
        (&[(extensions, &trailing)], false),
    ];
    let mut task_ids = Vec::new();
    for (index, (headers, activated)) in cases.into_iter().enumerate() {
        let (response, events) = stream_message_with(
            &door.addr,
            None,
            json!(index),
            message(&format!("m-{index}"), "hi"),
            headers,
        );
        assert_eq!(
            response.header("a2a-extensions"),
            activated.then_some(STREAM_EXTENSION_URI),
            "{headers:?}"
        );
        assert_eq!(
            events.iter().any(Event::is_frame),
            activated,
            "{headers:?}: {events:#?}"
        );
        assert_eq!(response.body.contains(STREAM_EXTENSION_URI), activated);
        task_ids.push(events[0].task_id().unwrap().to_string());
    }

    let header = [A2A_VERSION, extensions_header()];
    for (method, params) in [
        ("GetTask", json!({"id": task_ids[0]})),
        ("SendMessage", message("m-unary", "hi")),
    ] {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let response = request(&door.addr, "POST", "/", &header, &body);
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(response.header("a2a-extensions"), None, "{method}");
        assert!(response.json().get("result").is_some(), "{response:?}");
    }
}

/// A stream ends once its task stops for input, carrying the prompt; the reply naming the task
/// continues it on a new stream that opens with the task working, resumes after the client's
/// cursor and ends after `COMPLETED`. Resuming from `0` replays the task's earlier events after
/// the opening task without ending on the replayed `INPUT_REQUIRED`.
#[test]
fn input_required_ends_the_stream_and_the_reply_continues_it() {
    let door = Door::request_input(common::ScriptedServer::start(vec![
        ask_for_input(1, "Which branch?"),
        end_turn(2, "main it is"),
        ask_for_input(3, "Which file?"),
        end_turn(4, "lib.rs it is"),
    ]));

    // The first task: the reply resumes after the last id the first stream saw.
    let (_, first) = door.stream(message("m-ask-1", "deploy"), false);
    let task_id = first[0].task_id().unwrap().to_string();
    let last = first.last().unwrap();
    assert_eq!(
        last.state(),
        Some("TASK_STATE_INPUT_REQUIRED"),
        "{first:#?}"
    );
    // The fixture tool asks with its whole input as the prompt.
    assert_eq!(
        last.status_text(),
        Some(json!({"data": "Which branch?"}).to_string().as_str())
    );
    let cursor = first.iter().filter_map(|event| event.id).max().unwrap();

    let cursor_header = cursor.to_string();
    let (_, continued) = stream_message_with(
        &door.addr,
        None,
        json!(2),
        reply(&task_id, "main"),
        &[("Last-Event-ID", &cursor_header)],
    );
    assert_one_task_stream(&door, &continued, "TASK_STATE_WORKING");
    assert!(
        continued
            .iter()
            .filter_map(|event| event.id)
            .all(|id| id > cursor),
        "an id at or below the cursor was written: {continued:#?}"
    );
    assert!(
        continued
            .iter()
            .any(|event| event.status_text() == Some("resumed")),
        "{continued:#?}"
    );
    assert_eq!(
        continued.last().unwrap().state(),
        Some("TASK_STATE_COMPLETED")
    );

    // The second task: an activated reply resuming from 0 replays the task's earlier events.
    let (_, second) = door.stream(message("m-ask-2", "edit"), false);
    let second_id = second[0].task_id().unwrap().to_string();
    assert_eq!(
        second.last().unwrap().state(),
        Some("TASK_STATE_INPUT_REQUIRED")
    );
    let (_, replayed) = stream_message_with(
        &door.addr,
        None,
        json!(3),
        reply(&second_id, "lib.rs"),
        &[("Last-Event-ID", "0"), extensions_header()],
    );
    assert_one_task_stream(&door, &replayed, "TASK_STATE_WORKING");
    let interrupted = replayed
        .iter()
        .position(|event| event.state() == Some("TASK_STATE_INPUT_REQUIRED"))
        .expect("the replay holds the INPUT_REQUIRED status");
    assert!(replayed[interrupted].id.is_some());
    assert!(
        interrupted < replayed.len() - 1,
        "the stream did not end on the replayed INPUT_REQUIRED"
    );
    assert!(
        replayed
            .iter()
            .any(|event| event.status_text() == Some("inference turn 1")),
        "the replay holds the task's first turn: {replayed:#?}"
    );
    assert_eq!(
        replayed.last().unwrap().state(),
        Some("TASK_STATE_COMPLETED")
    );
}

/// A `SendStreamingMessage` stream carries only its own task, even activated and with another
/// task running, while a `murmur.WatchSession` opened first sees both and ends on neither.
#[test]
fn own_task_only_on_a_message_stream_and_every_task_on_watch() {
    let door = Door::anthropic(
        "own-task",
        common::ScriptedServer::start_with_delay(
            vec![end_turn(1, "first"), end_turn(2, "second")],
            Duration::from_millis(1500),
        ),
        QUEUE_SLEEP_YAML,
    );
    let mut watch = Watch::open(&door.addr, 0);
    assert!(
        watch.read_until(Duration::from_secs(30), |event| event.kind()
            == "connection-ack")
    );
    assert_eq!(
        watch.header("a2a-extensions"),
        Some(format!("{}, {STREAM_EXTENSION_URI}", common::DOOR_EXTENSION_URI).as_str())
    );

    let first = door.rpc("SendMessage", message("m-a", "a"))["result"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, events) = door.stream(message("m-b", "b"), true);
    let second = assert_one_task_stream(&door, &events, "TASK_STATE_SUBMITTED");
    assert!(
        events
            .iter()
            .all(|event| event.task_id() != Some(first.as_str())),
        "{events:#?}"
    );
    assert!(
        !serde_json::to_string(&events.iter().map(|e| &e.json).collect::<Vec<_>>())
            .unwrap()
            .contains(&first)
    );

    assert!(
        watch.read_until(Duration::from_secs(60), |event| event.is_terminal_status()
            && event.task_id() == Some(second.as_str()))
    );
    for task in [&first, &second] {
        assert!(
            of_task(&watch.events, task)
                .iter()
                .any(|event| event.is_terminal_status()),
            "the watch saw {task} end"
        );
    }
    assert!(
        !watch.read_until(Duration::from_secs(1), |_| true) && !watch.closed,
        "the watch ended on a task's terminal status"
    );
}

/// A busy door answers a `SendStreamingMessage` with one unnumbered `TASK_STATE_REJECTED` task
/// event, the answer `SendMessage` gives the same refusal; a malformed message is a `-32602`
/// JSON body before any stream.
#[test]
fn refusal_busy_rejects_in_one_task_event_and_malformed_is_json() {
    let door = Door::anthropic(
        "refusal",
        common::ScriptedServer::start_with_delay(
            vec![end_turn(1, "slow"), end_turn(2, "spare")],
            Duration::from_secs(3),
        ),
        &QUEUE_SLEEP_YAML.replace("queue_depth: 8", "queue_depth: 1"),
    );
    // One task running and one queued fill a queue of one.
    let running = door.rpc("SendMessage", message("m-1", "one"))["result"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    while door.get_task(&running)["status"]["state"] != "TASK_STATE_WORKING" {
        assert!(Instant::now() < deadline, "the first task never started");
        thread::sleep(Duration::from_millis(20));
    }
    door.rpc("SendMessage", message("m-2", "two"));

    let (response, events) = door.stream(message("m-3", "three"), true);
    assert_eq!(response.status, 200);
    assert!(response
        .header("content-type")
        .is_some_and(|value| value.starts_with("text/event-stream")));
    assert_eq!(events.len(), 1, "{events:#?}");
    let rejected = &events[0];
    assert_eq!(rejected.id, None);
    assert_eq!(rejected.kind(), "task");
    assert_eq!(rejected.state(), Some("TASK_STATE_REJECTED"));
    assert_eq!(
        rejected.status_text(),
        Some("task rejected: capsule is busy")
    );

    let (response, events) = door.stream(
        json!({"message": {"messageId": "m-4",
        "role": "ROLE_USER", "parts": []}}),
        false,
    );
    assert!(events.is_empty());
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.json()["error"]["code"], -32602, "{response:?}");
}

/// A door that starts no task does not stream: its card says so, and it answers
/// `SendStreamingMessage` `-32004` `UnsupportedOperationError`.
#[test]
fn refusal_a_door_that_starts_no_task_answers_unsupported_operation() {
    // A door that starts no task is up while its `task.md` runs, which the provider holds.
    let provider = common::ScriptedServer::start_with_delay(
        vec![end_turn(1, "done")],
        Duration::from_secs(120),
    );
    let home = driver_home();
    let project = agent_project(
        &provider.endpoint,
        "no-tasks",
        "",
        "lifecycle:\n  task_acceptance: none\n  after_task: exit\n",
    );
    let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
    fs::write(staged.workdir.join("task.md"), "Run the task").unwrap();
    let door = Door {
        addr: launch_door(staged),
        _provider: Some(provider),
        _home: home,
        _project: project,
    };
    let card = request(&door.addr, "GET", "/.well-known/agent-card.json", &[], "").json();
    assert_eq!(card["capabilities"]["streaming"], false, "{card}");

    let (response, events) = door.stream(message("m-none", "hi"), true);
    assert!(events.is_empty());
    assert_eq!(response.header("content-type"), Some("application/json"));
    let error = &response.json()["error"];
    assert_eq!(error["code"], -32004, "{response:?}");
    assert_eq!(error["data"][0]["reason"], "UNSUPPORTED_OPERATION");
}

/// The card of a door that streams says so, and its stream extension lists every murmur frame
/// with the description of what activation does.
#[test]
fn card_says_what_the_stream_extension_adds() {
    let door = Door::anthropic(
        "card",
        common::ScriptedServer::start(vec![]),
        QUEUE_SLEEP_YAML,
    );
    let card = request(&door.addr, "GET", "/.well-known/agent-card.json", &[], "").json();
    assert_eq!(card["capabilities"]["streaming"], true, "{card}");
    let stream = card["capabilities"]["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|extension| extension["uri"] == STREAM_EXTENSION_URI)
        .unwrap();
    assert_eq!(
        stream["params"]["frames"],
        json!([
            "text",
            "thinking",
            "tool-call-started",
            "tool-call-progress",
            "artifact",
            "gap",
            "lagged",
            "connection-ack",
            "capsule-closed"
        ])
    );
    assert_eq!(stream["required"], false);
    assert_eq!(
        stream["description"],
        "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on \
         SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names \
         this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata \
         keyed by this URI."
    );
}
