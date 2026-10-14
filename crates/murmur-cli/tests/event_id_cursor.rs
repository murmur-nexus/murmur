//! Integration tests for the session-wide event-id sequence against a real launched capsule.
//!
//! Every numbered frame a capsule session writes takes the next id of one sequence, whatever
//! task it belongs to, so `Last-Event-ID` resumes exactly after the frame it names.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::idle_capsule::{
    end_turn, end_turn_server, end_turns, http_post_json, launch_idle_capsule,
    launch_idle_capsule_with, submit_task, wait_for_task_ends, IdleCapsule,
};
use common::stream::{Event, Watch};
use serde_json::Value;

/// The tool name the model calls, which is also the name the artifact is packed under.
const TOOL_NAME: &str = "request-input-tool";
/// The tool's full name once installed from its `.mur.zip` file.
const TOOL_ARTIFACT: &str = "local/request-input-tool";
const TOOL_VERSION: &str = "0.1.0";
const REOPEN_HOOK_NAME: &str = "local/reopen-every-task";

// ── capsule ────────────────────────────────────────────────────────────────────

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

/// Launch a capsule that also declares `request-input-tool` and an `on-task-end` hook that reopens
/// every task, once each under the default `lifecycle.max_task_reopens`.
fn launch_with_input_tool_and_reopen_hook(server: common::ScriptedServer) -> IdleCapsule {
    launch_idle_capsule_with(server, |home, dir| {
        common::publish_local(home, &create_tool_artifact(dir)).success();
        let hook = common::hook_wat::create_hook_zip(
            dir,
            REOPEN_HOOK_NAME,
            "on-task-end",
            "reopen-task",
            &common::hook_wat::reopen_task_hook_wasm("once more"),
        );
        common::publish_local(home, &hook).success();
        format!(
            "  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n    runtime: tool\n  \
             - name: {REOPEN_HOOK_NAME}\n    version: 0.1.0\n    runtime: hook\n"
        )
    })
}

// ── tasks and trace ────────────────────────────────────────────────────────────

fn rpc(addr: &str, method: &str, params: Value) -> Value {
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    http_post_json(addr, &body.to_string())
}

fn wait_for_state(addr: &str, task_id: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = rpc(addr, "GetTask", serde_json::json!({"id": task_id}));
        if response["result"]["status"]["state"] == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {task_id} to reach {expected}; last: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The session trace's records of `event_type`.
fn trace_records(workdir: &Path, event_type: &str) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["event_type"] == event_type)
        .collect()
}

/// Wait until the trace holds `task_id`'s `task_end`.
fn wait_for_task_end(workdir: &Path, task_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !trace_records(workdir, "task_end")
        .iter()
        .any(|record| record["task_id"] == task_id)
    {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {task_id} to end"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── events ─────────────────────────────────────────────────────────────────────

/// Launch a capsule, run two tasks to completion and return it with the full replay a watcher
/// attached with `Last-Event-ID: 0` receives.
fn two_tasks_replayed() -> (IdleCapsule, Vec<Event>) {
    let capsule = launch_idle_capsule(end_turn_server(&["first reply", "second reply"]));
    submit_task(&capsule.url, "event-id-cursor-1", "first task");
    wait_for_task_ends(&capsule.workdir, 1);
    submit_task(&capsule.url, "event-id-cursor-2", "second task");
    wait_for_task_ends(&capsule.workdir, 2);

    let replay = watch_frames(&capsule.url, 0);
    (capsule, replay)
}

/// The events a `murmur.WatchSession` attached with `last_event_id` receives within 3 s after its
/// `connection-ack`, which is asserted to come first.
fn watch_frames(addr: &str, last_event_id: u64) -> Vec<Event> {
    let mut watch = Watch::open(addr, last_event_id);
    watch.read_until(Duration::from_secs(3), |_| false);
    let all = watch.events;
    assert_eq!(
        all.first().map(Event::kind).as_deref(),
        Some("connection-ack"),
        "murmur.WatchSession should open with connection-ack; events were {all:#?}"
    );
    all.into_iter().skip(1).collect()
}

/// The distinct task ids in the order their first event was written.
fn task_order(events: &[Event]) -> Vec<String> {
    let mut order = Vec::new();
    for task in events.iter().filter_map(Event::task_id) {
        if !order.iter().any(|known| known == task) {
            order.push(task.to_string());
        }
    }
    order
}

fn is_final_status_of(event: &Event, task_id: &str) -> bool {
    event.is_terminal_status() && event.task_id() == Some(task_id)
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
        replay.iter().all(|f| f.kind() != "gap"),
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
    assert_eq!(
        first.kind(),
        "status-update",
        "id 1 should be a status: {first:?}"
    );
    assert_eq!(
        first.murmur_state(),
        Some("working"),
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
            .filter(|f| f.task_id() == Some(task))
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
        .find(|f| is_final_status_of(f, &tasks[0]))
        .and_then(|f| f.id)
        .unwrap_or_else(|| panic!("no final status for the first task; frames were {replay:#?}"));

    let resumed = watch_frames(&capsule.url, cursor);

    assert!(
        resumed.iter().all(|f| f.kind() != "gap"),
        "resuming from a buffered id should carry no gap; frames were {resumed:#?}"
    );
    let expected: Vec<Event> = replay
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
        .filter(|f| f.task_id() == Some(tasks[1].as_str()))
        .count();
    assert_eq!(
        resumed
            .iter()
            .filter(|f| f.task_id() == Some(tasks[1].as_str()))
            .count(),
        second_task_frames,
        "the resumed replay should include every frame of the second task"
    );
    assert!(
        resumed
            .iter()
            .all(|f| f.task_id() != Some(tasks[0].as_str())),
        "the resumed replay should include no frame of the first task; frames were {resumed:#?}"
    );
}

/// A client that drops its connection mid-task and reconnects with the last id it saw, while a
/// later task is running, receives every frame written after that id exactly once — the rest of
/// the first task and the later task, replayed and then live — and nothing written before it.
#[test]
fn reconnect_mid_task_receives_every_later_frame_once() {
    if common::skip_without_host_support("reconnect_mid_task_receives_every_later_frame_once") {
        return;
    }
    // Each reply takes 2 s, so both the disconnect and the reconnect land inside a task.
    let capsule = launch_idle_capsule(common::ScriptedServer::start_with_delay(
        end_turns(&["first reply", "second reply"]),
        Duration::from_secs(2),
    ));
    let first = submit_task(&capsule.url, "event-id-mid-1", "first task");

    let mut dropped = Watch::open(&capsule.url, 0);
    dropped.read_until(Duration::from_secs(30), |event| event.id.is_some());
    let before = std::mem::take(&mut dropped.events);
    drop(dropped);
    let last_seen = before
        .iter()
        .filter_map(|f| f.id)
        .next_back()
        .unwrap_or_else(|| panic!("no numbered frame before the disconnect: {before:#?}"));
    assert!(
        !before.iter().any(|f| is_final_status_of(f, &first)),
        "the connection should drop mid-task; frames were {before:#?}"
    );

    let second = submit_task(&capsule.url, "event-id-mid-2", "second task");
    wait_for_task_end(&capsule.workdir, &first);
    wait_for_state(&capsule.url, &second, "TASK_STATE_WORKING");

    let mut resumed_conn = Watch::open(&capsule.url, last_seen);
    resumed_conn.read_until(Duration::from_secs(30), |event| {
        is_final_status_of(event, &second)
    });
    let resumed = resumed_conn.events;
    assert!(
        trace_records(&capsule.workdir, "task_end").len() == 2,
        "the second task should have finished while the client was reconnected"
    );
    let resumed: Vec<Event> = resumed
        .into_iter()
        .filter(|f| f.kind() != "connection-ack")
        .collect();
    assert!(
        resumed.iter().all(|f| f.kind() != "gap"),
        "nothing was evicted, so no gap; frames were {resumed:#?}"
    );

    let everything = watch_frames(&capsule.url, 0);
    let expected: Vec<Event> = everything
        .iter()
        .filter(|f| f.id.is_some_and(|id| id > last_seen))
        .cloned()
        .collect();
    assert_eq!(
        resumed, expected,
        "the reconnected client should receive exactly the frames after id {last_seen}, once each"
    );
    assert!(
        resumed.iter().any(|f| is_final_status_of(f, &first))
            && resumed
                .iter()
                .any(|f| f.task_id() == Some(second.as_str())
                    && f.murmur_state() == Some("working")),
        "the rest of the first task and the start of the second should both arrive; frames were {resumed:#?}"
    );
}

/// One session holding two tasks that each reopen once, a `request-input` wait and a queued
/// task cancelled before it started numbers every frame from one sequence: `1..=n`, no repeat, no
/// jump.
#[test]
fn every_kind_of_frame_shares_one_sequence() {
    if common::skip_without_host_support("every_kind_of_frame_shares_one_sequence") {
        return;
    }
    // The first reply is delayed so the queued task is submitted and cancelled while the first
    // task is still in inference, not while it waits for input.
    let capsule = launch_with_input_tool_and_reopen_hook(common::ScriptedServer::start_with_delay(
        vec![
            request_input_call(1),
            end_turn(2, "first task"),
            end_turn(3, "first task, reopened"),
            end_turn(4, "third task"),
            end_turn(5, "third task, reopened"),
        ],
        Duration::from_millis(500),
    ));

    let first = submit_task(&capsule.url, "event-id-all-1", "first task");
    let queued = submit_task(&capsule.url, "event-id-all-2", "cancelled while queued");
    let canceled = rpc(
        &capsule.url,
        "CancelTask",
        serde_json::json!({"id": queued}),
    );
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "the queued task should cancel; got {canceled}"
    );

    wait_for_state(&capsule.url, &first, "TASK_STATE_INPUT_REQUIRED");
    let input = rpc(
        &capsule.url,
        "SendMessage",
        serde_json::json!({"message": {
            "messageId": "event-id-all-input",
            "taskId": first,
            "role": "ROLE_USER",
            "parts": [{"text": "option A", "mediaType": "text/plain"}]
        }, "configuration": {"returnImmediately": true}}),
    );
    assert!(
        input["result"]["task"]["id"] == first.as_str(),
        "the reply should go to the waiting task; got {input}"
    );
    wait_for_task_end(&capsule.workdir, &first);

    let third = submit_task(&capsule.url, "event-id-all-3", "third task");
    wait_for_task_end(&capsule.workdir, &third);

    let reopened = trace_records(&capsule.workdir, "task_reopened");
    assert_eq!(
        reopened.len(),
        2,
        "each task should reopen once: {reopened:?}"
    );

    let replay = watch_frames(&capsule.url, 0);
    let ids: Vec<u64> = replay
        .iter()
        .map(|f| {
            f.id.unwrap_or_else(|| panic!("replayed frame without an id: {f:?}"))
        })
        .collect();
    assert_eq!(
        ids,
        (1..=ids.len() as u64).collect::<Vec<_>>(),
        "ids should be exactly 1..=n across every task and frame kind; frames were {replay:#?}"
    );

    let of_task = |task: &str| {
        replay
            .iter()
            .filter(|f| f.task_id() == Some(task))
            .collect::<Vec<_>>()
    };
    assert!(
        of_task(&first)
            .iter()
            .any(|f| f.murmur_state() == Some("input-required")),
        "expected {first} to wait for input; frames were {replay:#?}"
    );
    for (task, said) in [
        (&first, "resumed"),
        (&queued, "task canceled before it started"),
    ] {
        assert!(
            of_task(task).iter().any(|f| f.status_text() == Some(said)),
            "expected a status of {task} saying {said}; frames were {replay:#?}"
        );
    }
    // Two attempts per task: one reopen boundary between them. Turns number straight through
    // both attempts, so each task has exactly one first turn.
    let count = |task: &str, said: &dyn Fn(&str) -> bool| {
        of_task(task)
            .iter()
            .filter(|f| f.status_text().is_some_and(said))
            .count()
    };
    for task in [&first, &third] {
        assert_eq!(
            count(task, &|text| text.starts_with("reopened by hook ")),
            1,
            "{task} should start two attempts; frames were {replay:#?}"
        );
        assert_eq!(
            count(task, &|text| text == "inference turn 1"),
            1,
            "{task} should have one first turn across its attempts; frames were {replay:#?}"
        );
    }
}
