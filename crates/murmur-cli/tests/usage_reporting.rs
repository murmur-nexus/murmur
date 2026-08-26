//! End-to-end coverage for the optional `usage` block of the driver response contract.
//!
//! These tests drive a real capsule session through the host `run_agent_loop` against a
//! purpose-built WASM driver fixture (`usage-driver`) that returns constant token counts and
//! never makes an HTTP call. A person can read the resulting `trace.jsonl` and see the
//! runtime's own estimate and the provider's reported counts side by side on one line.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::Value;

const DRIVER_NAME: &str = "usage-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The constants the fixture reports, mirrored from
/// `tests/fixtures/usage-driver/src/usage-driver/src/lib.rs`.
const REPORTED_INPUT_TOKENS: u64 = 12043;
const REPORTED_OUTPUT_TOKENS: u64 = 218;
const REPORTED_CACHED_TOKENS: u64 = 11780;
const REPORTED_CACHE_WRITE_TOKENS: u64 = 7;

/// The four keys the driver's own counts land on. Named once here because a later card and
/// the trace consumers read them by key.
const ACTUAL_KEYS: [&str; 4] = [
    "input_tokens_actual",
    "output_tokens_actual",
    "cached_tokens",
    "cache_write_tokens",
];

fn driver_wasm() -> PathBuf {
    common::fixture_path("usage-driver/tool/usage-driver.wasm")
}

/// Build a project manifest wiring in the usage-driver. `config_mode` (when `Some`) is passed
/// through `inference.driver.config` and reaches the driver as
/// `MURMUR_INFERENCE_DRIVER_CONFIG`, selecting which `usage` shape it returns.
///
/// The capsule declares no `capabilities.shell.allow`: the fixture ends the task on turn 0
/// and never calls a tool, so the session spawns no subprocess and needs no host gate.
fn create_manifest(project_dir: &Path, config_mode: Option<&str>) -> PathBuf {
    // Dummy endpoint: the driver answers locally and never makes an HTTP request.
    let endpoint = "http://127.0.0.1:9";
    let config_section = match config_mode {
        Some(mode) => format!("    config:\n      mode: {mode}\n"),
        None => String::new(),
    };
    let manifest = format!(
        concat!(
            "name: agent-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "{config_section}",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        config_section = config_section,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

struct Run {
    result: String,
    trace: String,
    bootstrap_log: String,
}

/// Publish the driver, stage a session, write `task.md`, launch, and collect what the session
/// left behind.
fn run_session(config_mode: Option<&str>) -> Run {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &driver_wasm(),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_manifest(project.path(), config_mode);
    let staged: StagedSession = common::stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Report your usage.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    Run {
        result: fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap_or_default(),
        trace: fs::read_to_string(launched.workdir.join("trace.jsonl")).unwrap_or_default(),
        bootstrap_log: fs::read_to_string(launched.workdir.join("logs/bootstrap.log"))
            .unwrap_or_default(),
    }
}

fn events(trace: &str, event_type: &str) -> Vec<Value> {
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|ev| ev.get("event_type").and_then(Value::as_str) == Some(event_type))
        .collect()
}

/// The session's single `inference` event.
fn sole_inference(trace: &str) -> Value {
    let inferences = events(trace, "inference");
    assert_eq!(
        inferences.len(),
        1,
        "the fixture ends the task on turn 0, so exactly one inference turn is expected; \
         got: {inferences:?}"
    );
    inferences.into_iter().next().unwrap()
}

fn exit_status(trace: &str) -> String {
    events(trace, "session_end")
        .last()
        .and_then(|ev| ev.get("exit_status").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// Happy path: the driver's numbers reach the trace verbatim, beside — never in place of —
/// the runtime's own tiktoken estimates.
#[test]
fn usage_block_is_recorded_verbatim() {
    let run = run_session(None);
    assert_eq!(exit_status(&run.trace), "ok", "trace: {}", run.trace);

    let inference = sole_inference(&run.trace);
    assert_eq!(inference["input_tokens_actual"], REPORTED_INPUT_TOKENS);
    assert_eq!(inference["output_tokens_actual"], REPORTED_OUTPUT_TOKENS);
    assert_eq!(inference["cached_tokens"], REPORTED_CACHED_TOKENS);
    assert_eq!(
        inference["cache_write_tokens"], REPORTED_CACHE_WRITE_TOKENS,
        "a reported value is recorded even when it is zero-adjacent"
    );

    // The estimate survives the actual: both numbers are on the line, and they are the two
    // different measurements they claim to be — one a tiktoken count of a JSON string, the
    // other the provider's own count.
    let estimated_input = inference["input_tokens"]
        .as_u64()
        .expect("estimate present");
    let estimated_output = inference["output_tokens"]
        .as_u64()
        .expect("estimate present");
    assert!(estimated_input > 0, "line: {inference}");
    assert_ne!(
        estimated_input, REPORTED_INPUT_TOKENS,
        "the actual must not have overwritten the estimate"
    );
    assert_ne!(
        estimated_output, REPORTED_OUTPUT_TOKENS,
        "the actual must not have overwritten the estimate"
    );

    // The estimate, not the report, is what the session totals accumulate.
    let session_end = events(&run.trace, "session_end");
    let session_end = session_end.last().expect("session_end present");
    assert_eq!(session_end["total_input_tokens"], estimated_input);
    assert_eq!(session_end["total_output_tokens"], estimated_output);
}

/// A driver that reports nothing runs exactly as it did before `usage` existed: no key, no
/// zero, no null, and no complaint anywhere.
#[test]
fn absent_usage_is_absent_not_zero() {
    let run = run_session(Some("NOUSAGE"));
    assert_eq!(exit_status(&run.trace), "ok", "trace: {}", run.trace);
    assert!(!run.result.contains("error"), "result: {:?}", run.result);

    let inference = sole_inference(&run.trace);
    for key in ACTUAL_KEYS {
        assert!(
            inference.get(key).is_none(),
            "{key} must be absent, not null and not 0; line: {inference}"
        );
    }
    assert!(
        inference["input_tokens"].as_u64().unwrap_or(0) > 0,
        "the runtime's own estimate is unaffected; line: {inference}"
    );
    assert!(
        !run.bootstrap_log.contains("usage"),
        "a driver that reports no usage must not be warned about; log: {}",
        run.bootstrap_log
    );
}

/// A `usage` the host cannot make sense of degrades to "absent" — every ill-typed member
/// dropped, nothing panicked, nothing failed.
#[test]
fn malformed_usage_degrades_to_absent() {
    for mode in ["BADUSAGE", "NONOBJECTUSAGE"] {
        let run = run_session(Some(mode));
        assert_eq!(
            exit_status(&run.trace),
            "ok",
            "mode {mode} must not fail the session; trace: {}",
            run.trace
        );
        assert!(
            !run.result.contains("error"),
            "mode {mode} must not put error text in the result; got: {:?}",
            run.result
        );

        let inference = sole_inference(&run.trace);
        for key in ACTUAL_KEYS {
            assert!(
                inference.get(key).is_none(),
                "mode {mode}: {key} must be absent; line: {inference}"
            );
        }
    }
}
