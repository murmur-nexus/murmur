//! A work-folder breach decides the session's status even when the guard's poll never saw it.
//!
//! The guard that enforces `capabilities.resources.workdir_max_bytes` polls the folders on a
//! thread. A command that writes past the ceiling between two polls, and a model that answers
//! right after, used to end the session `ok` with the breach line printed afterwards and no
//! `E-RUN-013`: the answer was final before the monitor had reported. The runtime now measures
//! once more, synchronously, where the status is decided.
//!
//! This test proves that, not by running it many times and hoping the poll is late, but by
//! holding the poll shut: the `workdir.poll` race window (`capsule-runtime/src/race.rs`) stalls
//! the polling thread for the whole run, so the final measurement is the only thing that can
//! find the breach. Without that measurement the session ends `ok` and this test fails, every
//! time.
//!
//! It needs the `race-windows` feature, which `scripts/test.sh` turns on.
#![cfg(feature = "race-windows")]

#[path = "common/mod.rs"]
mod common;

use std::fs;

use capsule_runtime::launch_session;
use common::ScriptedServer;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CEILING: u64 = 1024 * 1024;

fn response(id: &str, tool_command: Option<&str>, text: &str) -> String {
    let (content, stop_reason) = match tool_command {
        Some(command) => (
            json!([{"type": "tool_use", "id": format!("toolu_{id}"), "name": "bash",
                    "input": {"command": command}}]),
            "tool_use",
        ),
        None => (json!([{"type": "text", "text": text}]), "end_turn"),
    };
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// Runs one session whose only command writes `bytes` into the work folder, then answers. The
/// polling thread of the workdir guard is held shut for the whole run.
fn run_with_the_poll_held(bytes: usize) -> (Result<(), String>, String) {
    // Read once, when the first window is reached, so it is set once and before any launch.
    static HOLD_THE_POLL: std::sync::Once = std::sync::Once::new();
    HOLD_THE_POLL.call_once(|| std::env::set_var("MURMUR_RACE_WINDOWS", "workdir.poll=600000"));

    let write = format!(
        "block=$(printf '%01024d' 0); for ((i = 0; i < {}; i++)); do printf '%s' \"$block\"; \
         done > big.bin",
        bytes / 1024
    );
    let server = ScriptedServer::start_with_delay(
        vec![
            response("msg_1", Some(&write), ""),
            response("msg_2", None, "all done"),
        ],
        std::time::Duration::ZERO,
    );
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let endpoint = &server.endpoint;
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: breach-window\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
             endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\n  resources:\n    \
             workdir_max_bytes: {CEILING}\ninference:\n  transport: http\n  model: test-model\n  \
             driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    let staged =
        common::stage_agent_session_with_workdir(&home, project.path(), &manifest, work.path());
    let records = staged.records_dir.clone();
    staged.write_task_input(b"Write the file.").unwrap();

    let outcome = launch_session(staged, |_| {})
        .map(|_| ())
        .map_err(|error| error.to_string());
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap_or_default();
    (outcome, trace)
}

#[test]
fn a_breach_the_poll_never_saw_still_ends_the_session_with_the_ceiling_error() {
    if common::skip_without_host_support(
        "a_breach_the_poll_never_saw_still_ends_the_session_with_the_ceiling_error",
    ) {
        return;
    }
    let (outcome, trace) = run_with_the_poll_held(2 * CEILING as usize);
    let error = outcome.expect_err(
        "the session wrote past its ceiling and ended ok: the status was decided before the \
         workdir measurement was in",
    );
    assert!(
        error.contains("past the 1048576 byte ceiling (capabilities.resources.workdir_max_bytes)"),
        "{error}"
    );
    assert!(
        trace.contains("task_failed"),
        "the breach leaves a task_failed line:\n{trace}"
    );
}

/// The other half: with the poll held shut the final measurement does not invent a breach.
#[test]
fn a_session_under_its_ceiling_still_ends_ok_with_the_poll_held() {
    if common::skip_without_host_support(
        "a_session_under_its_ceiling_still_ends_ok_with_the_poll_held",
    ) {
        return;
    }
    let (outcome, _trace) = run_with_the_poll_held(4 * 1024);
    outcome.expect("a session under its ceiling ends ok");
}
