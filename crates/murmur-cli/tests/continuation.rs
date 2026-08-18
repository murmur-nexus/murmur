//! End-to-end coverage for stateful-driver continuation.
//!
//! These tests drive a real capsule session through the host `run_agent_loop` against a
//! purpose-built WASM driver fixture (`continuation-driver`) that echoes what it actually
//! saw on the wire into the Task's final text. A person can read `out/result.txt` and confirm
//! incremental-vs-full behavior directly, without reading the host source.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::Value;

const DRIVER_NAME: &str = "continuation-driver";
const DRIVER_VERSION: &str = "0.1.0";

fn driver_wasm() -> PathBuf {
    common::fixture_path("continuation-driver/tool/continuation-driver.wasm")
}

/// Build a project manifest wiring in the continuation-driver. `config_mode` (when `Some`)
/// is passed through `inference.driver.config` and reaches the driver as
/// `MURMUR_INFERENCE_DRIVER_CONFIG` — it never appears in the request payload, so it toggles
/// continuation opt-in without changing any logical message content.
fn create_manifest(project_dir: &Path, config_mode: Option<&str>) -> PathBuf {
    // Dummy endpoint: the driver echoes locally and never makes an HTTP request.
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
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
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

/// Publish the driver, stage a session, write `task.md`, launch, and return
/// `(result_txt, trace_jsonl)`.
fn run_session(config_mode: Option<&str>) -> (String, String) {
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

    // Identical task text for both modes — the only difference between runs is the driver
    // config toggle, so the logical `messages` content (and thus token accounting) matches.
    fs::write(staged.workdir.join("task.md"), "Please echo hello.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    let result = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap_or_default();
    let trace = fs::read_to_string(launched.workdir.join("trace.jsonl")).unwrap_or_default();
    (result, trace)
}

/// Per-turn `input_tokens` from the trace's `inference` events, in turn order.
fn inference_input_tokens(trace: &str) -> Vec<u64> {
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|ev| ev.get("event_type").and_then(Value::as_str) == Some("inference"))
        .filter_map(|ev| ev.get("input_tokens").and_then(Value::as_u64))
        .collect()
}

/// Scenario 2: driver opts in on Turn 0 → Turn 1 wire payload is incremental (only the
/// assistant + tool-result messages appended since Turn 0) and carries the continuation id.
#[test]
fn continuation_active_sends_incremental_second_turn() {
    if common::skip_without_cgroup_delegation("continuation_active_sends_incremental_second_turn") {
        return;
    }
    let (result, _trace) = run_session(None);
    assert_eq!(
        result.trim(),
        "wire_n=2 cont=cont-echo-1",
        "Turn 1 should transmit only the 2 messages appended since Turn 0 (assistant + tool \
         result), plus the held continuation id; got: {result:?}"
    );
}

/// Scenario 1: a driver that never sets the metadata key sees zero behavior change — every
/// Turn resends the full `messages` array and no continuation id is ever attached.
#[test]
fn no_continuation_key_resends_full_history() {
    if common::skip_without_cgroup_delegation("no_continuation_key_resends_full_history") {
        return;
    }
    let (result, _trace) = run_session(Some("NOCONT"));
    assert_eq!(
        result.trim(),
        "wire_n=3 cont=none",
        "Turn 1 should resend the full 3-message history with no continuation id; got: {result:?}"
    );
}

/// Scenario 4: `session_tokens` input-token accounting is computed from the full logical
/// `messages` array regardless of what is transmitted. The continuation-active and
/// continuation-inactive runs share byte-identical logical message content, so the per-turn
/// *growth* in `input_tokens` (the contribution of the messages appended each turn) is
/// identical between them — even though the active run transmits a smaller incremental wire
/// payload. (Absolute counts differ only by a per-run constant: each run's system prompt
/// embeds its own random temp workdir path. Subtracting turn 0 cancels that offset. Were
/// input tokens computed from the smaller wire instead of the full `messages`, the active
/// run's growth would collapse and this equality would fail.)
#[test]
fn token_accounting_is_identical_with_and_without_continuation() {
    if common::skip_without_cgroup_delegation(
        "token_accounting_is_identical_with_and_without_continuation",
    ) {
        return;
    }
    let (cont_result, cont_trace) = run_session(None);
    let (full_result, full_trace) = run_session(Some("NOCONT"));

    // Sanity: the two runs genuinely differ on the wire (incremental vs full).
    assert_eq!(cont_result.trim(), "wire_n=2 cont=cont-echo-1");
    assert_eq!(full_result.trim(), "wire_n=3 cont=none");

    let cont_tokens = inference_input_tokens(&cont_trace);
    let full_tokens = inference_input_tokens(&full_trace);
    assert_eq!(
        cont_tokens.len(),
        full_tokens.len(),
        "both runs should record the same number of inference turns (active: {cont_tokens:?}, \
         inactive: {full_tokens:?})"
    );
    assert!(cont_tokens.len() >= 2, "expected at least two inference turns");

    let cont_growth: Vec<i64> = cont_tokens
        .iter()
        .map(|&t| t as i64 - cont_tokens[0] as i64)
        .collect();
    let full_growth: Vec<i64> = full_tokens
        .iter()
        .map(|&t| t as i64 - full_tokens[0] as i64)
        .collect();
    assert_eq!(
        cont_growth, full_growth,
        "per-turn input_token growth must match between continuation-active and \
         continuation-inactive runs with identical logical content (active: {cont_tokens:?}, \
         inactive: {full_tokens:?})"
    );
    // The growth must be strictly positive — the appended assistant + tool-result messages
    // contribute real tokens, confirming accounting used the full messages, not the empty/tiny
    // wire slice a broken implementation would have counted.
    assert!(
        cont_growth[1] > 0,
        "continuation-active run's turn-1 accounting must reflect the full appended messages, \
         got growth {cont_growth:?}"
    );
}
