//! Limits scenario 6 (`release-tools/limits.sh`), in-process and repeated: a command writes 1.8 GB
//! in 9 MB files against a 50 MiB `workdir_max_bytes`, and every one of the runs ends with
//! `E-RUN-013`.
//!
//! The scenario used to end `status: ok` in about half its runs: the breach was measured on a
//! poll the session could outrun, and the final status never asked again. The scripted model here
//! plays the part the real one plays in `limits.sh`, which runs the command and reports, with no
//! timing of its own: the guard's poll comes round when it comes round, as in production. The
//! forced-window proof of the same fix is `workdir_breach_window.rs`.
//!
//! A run also checks the writer was stopped by the breach, not left to finish: fewer than all 200
//! files exist when the session ends.

#[path = "common/mod.rs"]
mod common;

use std::fs;

use capsule_runtime::launch_session;
use common::ScriptedServer;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CEILING: u64 = 50 * 1024 * 1024;
/// The command `limits.sh` scenario 6 hands the model.
const SCENARIO_6: &str =
    "for i in $(seq 1 200); do dd if=/dev/zero of=./fill-$i bs=1M count=9 2>/dev/null; done";
const FILES: usize = 200;
/// How many sessions in a row must all end with the ceiling error.
const RUNS: usize = 25;

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

/// One session of scenario 6: the error it ended with, if any, and how many of the 200 files the
/// command had written when it ended.
fn run_scenario_6() -> (Result<(), String>, usize) {
    let server = ScriptedServer::start_with_delay(
        vec![
            response("msg_1", Some(SCENARIO_6), ""),
            response("msg_2", None, "exit code 0"),
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
            "name: scenario-6\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
             endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\n      - dd\n      \
             - seq\n  resources:\n    workdir_max_bytes: {CEILING}\ninference:\n  \
             transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    let staged =
        common::stage_agent_session_with_workdir(&home, project.path(), &manifest, work.path());
    staged.write_task_input(b"Run the command.").unwrap();
    let outcome = launch_session(staged, |_| {})
        .map(|_| ())
        .map_err(|error| error.to_string());
    let written = (1..=FILES)
        .filter(|i| work.path().join(format!("fill-{i}")).exists())
        .count();
    (outcome, written)
}

#[test]
fn scenario_6_ends_with_the_ceiling_error_every_time() {
    if common::skip_without_host_support("scenario_6_ends_with_the_ceiling_error_every_time") {
        return;
    }
    let mut wrong = Vec::new();
    for run in 1..=RUNS {
        let (outcome, written) = run_scenario_6();
        match outcome {
            Ok(()) => wrong.push(format!("run {run}: ended ok after {written} files")),
            Err(error)
                if !error.contains(
                    "past the 52428800 byte ceiling (capabilities.resources.workdir_max_bytes)",
                ) =>
            {
                wrong.push(format!("run {run}: ended with another error: {error}"));
            }
            Err(_) if written >= FILES => {
                wrong.push(format!(
                    "run {run}: the writer was not stopped ({written} files)"
                ));
            }
            Err(_) => {}
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {RUNS} runs were wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
