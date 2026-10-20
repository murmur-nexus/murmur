#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{
    hook_wat::{compaction_hook_wasm, create_hook_zip},
    ScriptedServer,
};

const DRIVER_ANTHROPIC_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// Stage the session from a manifest that includes context and compaction fields.
fn stage_compaction_session(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

fn two_turn_responses() -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_ls",
                "name": "bash",
                "input": {"command": "echo hello"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 50, "output_tokens": 50}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "Task complete."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 50, "output_tokens": 50}
        })
        .to_string(),
    ]
}

/// Threshold reached but murmur-compact not declared → warning logged, loop continues.
#[test]
fn compact_not_declared_threshold_reached_warning_logged() {
    if common::skip_without_host_support("compact_not_declared_threshold_reached_warning_logged") {
        return;
    }
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path =
        create_compaction_manifest(project.path(), &server.endpoint, DRIVER_ANTHROPIC_NAME);

    let staged = stage_compaction_session(&home, project.path(), &manifest_path);
    // Do NOT create murmur-compact tool dir → compact_tool_name = None

    fs::write(staged.workdir.join("task.md"), "Run echo and summarize.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    // out/result.txt must exist
    assert!(
        launched.workdir.join("out/result.txt").exists(),
        "out/result.txt should exist"
    );

    // summary.md must NOT exist (compaction was skipped)
    assert!(
        !launched.workdir.join("checkpoints/summary.md").exists(),
        "checkpoints/summary.md should NOT exist when murmur-compact is not installed"
    );

    // bootstrap.log must contain warning that no hook returned replace-context
    let log =
        fs::read_to_string(launched.records_dir.join("logs/bootstrap.log")).unwrap_or_default();
    assert!(
        log.contains("no hook returned replace-context"),
        "bootstrap.log should contain compaction skip warning, got: {log}"
    );

    // The decline also reaches the trace: a session that silently continued over budget is
    // indistinguishable in trace.jsonl from one that never needed compacting.
    let trace = fs::read_to_string(launched.records_dir.join("trace.jsonl")).unwrap_or_default();
    let events: Vec<serde_json::Value> = trace
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();
    let ids: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| e["event_id"].as_str())
        .collect();
    let declined: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["event_type"] == "compaction_declined")
        .collect();
    assert!(
        !declined.is_empty(),
        "the decline must be recorded as compaction_declined; got:\n{trace}"
    );
    let task_id = events
        .iter()
        .find(|e| e["event_type"] == "task_start")
        .expect("the session must have written task_start")["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    for d in declined {
        assert_eq!(d["reason"], "no_hook_replacement");
        let parent = d["parent_id"]
            .as_str()
            .expect("a declined compaction must name a parent");
        assert!(
            ids.contains(parent),
            "compaction_declined names parent_id {parent:?}, which is in no line: {d}"
        );
        assert!(
            d["turn"].as_u64().is_some(),
            "compaction_declined must name the tripping turn: {d}"
        );
        assert_eq!(d["task_id"], task_id.as_str());
        assert!(
            d["tokens"].as_u64().is_some_and(|t| t > 0),
            "compaction_declined must carry the over-budget occupancy: {d}"
        );
        // Every scripted response reports usage, so every decline was measured on it.
        assert_eq!(d["tokens_source"], "actual", "{d}");
    }

    // `mur trace show` reads the frame without erroring, and does not hide the decline
    // behind the `fired: no` line.
    assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .args([
            "trace",
            "show",
            launched.records_dir.join("trace.jsonl").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("Compaction"))
        .stdout(predicates::str::contains("declined"))
        .stdout(predicates::str::contains("no_hook_replacement"));
}

/// Create a test project manifest for compaction tests: context.max_tokens=200 and
/// compaction.threshold=0.01, so compaction fires on the very first turn, and bash allowed.
fn create_compaction_manifest(project_dir: &Path, endpoint: &str, driver_name: &str) -> PathBuf {
    // Build the manifest string with explicit indentation — never use `\` line
    // continuations in format! when indentation matters.
    let manifest = format!(
        concat!(
            "name: agent-capsule\n",
            "version: 0.1.0\n",
            "context:\n",
            "  max_tokens: 200\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "    gateway:\n",
            "      endpoint: {endpoint}\n",
            "      api_key: test-key\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "inference:\n",
            "  transport: http\n",
            "  model: test-model\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "  compaction:\n",
            "    threshold: 0.01\n",
        ),
        driver_name = driver_name,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();

    project_dir.join("murmur.yaml")
}

// ── Which number the trigger compares ────────────────────────────────────────

const COMPACTOR: &str = "local/compactor";
const SUMMARY: &str = "everything so far, in one line";
/// `context.max_tokens` and `inference.compaction.threshold` for the cases below: compaction
/// fires at 400 tokens, which the runtime's estimate of any request crosses on its own.
const WINDOW: u64 = 4_000;
const THRESHOLD: f64 = 0.1;

fn threshold_tokens() -> u64 {
    (WINDOW as f64 * THRESHOLD) as u64
}

/// An anthropic response: a `bash` call running `command`, or the end of the turn when `command`
/// is `None`, reporting `usage` when given.
fn response(command: Option<&str>, usage: Option<Value>) -> String {
    let (content, stop_reason) = match command {
        Some(command) => (
            json!([{
                "type": "tool_use",
                "id": "toolu_bash",
                "name": "bash",
                "input": {"command": command}
            }]),
            "tool_use",
        ),
        None => (
            json!([{"type": "text", "text": "Task complete."}]),
            "end_turn",
        ),
    };
    let mut response = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    response.to_string()
}

const TASK: &str = "Run the command and summarize.";

/// A capsule over `responses` with `context.max_tokens: window`, `compaction.threshold:
/// THRESHOLD`, `bash` allowed and, `with_hook`, [`compaction_hook_wasm`] bound to
/// `on-compaction`, run over `task` to its end. Returns every trace line.
fn run_compacting(responses: Vec<String>, window: u64, with_hook: bool, task: &str) -> Vec<Value> {
    let server = ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();
    let hook = if with_hook {
        let artifact = create_hook_zip(
            artifact_dir.path(),
            COMPACTOR,
            "on-compaction",
            "replace-context",
            &compaction_hook_wasm(SUMMARY),
        );
        common::publish_local(&home, &artifact).success();
        format!("  - name: {COMPACTOR}\n    version: 0.1.0\n    runtime: hook\n")
    } else {
        String::new()
    };

    let manifest =
        create_compaction_manifest(project.path(), &server.endpoint, DRIVER_ANTHROPIC_NAME);
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("  max_tokens: 200\n", &format!("  max_tokens: {window}\n"))
        .replace(
            "    threshold: 0.01\n",
            &format!("    threshold: {THRESHOLD}\n"),
        )
        .replace("capabilities:\n", &format!("{hook}capabilities:\n"));
    fs::write(&manifest, text).unwrap();

    let staged = stage_compaction_session(&home, project.path(), &manifest);
    fs::write(staged.workdir.join("task.md"), task).unwrap();
    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    let trace = fs::read_to_string(launched.records_dir.join("trace.jsonl")).unwrap();
    trace
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect()
}

fn of_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["event_type"] == event_type)
        .collect()
}

/// The agent loop's `inference` line for `turn`.
fn inference_at(events: &[Value], turn: u64) -> &Value {
    of_type(events, "inference")
        .into_iter()
        .find(|e| e["turn"] == turn && e.get("origin").is_none())
        .unwrap_or_else(|| panic!("no inference line for turn {turn}: {events:#?}"))
}

fn estimate(inference: &Value) -> u64 {
    inference["input_tokens"].as_u64().unwrap() + inference["output_tokens"].as_u64().unwrap()
}

/// The estimate crosses the threshold on turn 0 while the provider's count does not, so the
/// session carries on; turn 1's reported count crosses, and the compaction it fires records that
/// count as an `actual` one.
#[test]
fn compaction_fires_on_the_reported_count_not_the_estimate() {
    if common::skip_without_host_support("compaction_fires_on_the_reported_count_not_the_estimate")
    {
        return;
    }
    let events = run_compacting(
        vec![
            response(
                Some("echo one"),
                Some(json!({"input_tokens": 10, "output_tokens": 5})),
            ),
            response(
                Some("echo two"),
                Some(json!({
                    "input_tokens": 520,
                    "cache_read_input_tokens": 60,
                    "cache_creation_input_tokens": 20,
                    "output_tokens": 20
                })),
            ),
            response(None, Some(json!({"input_tokens": 30, "output_tokens": 5}))),
        ],
        WINDOW,
        true,
        TASK,
    );

    let turn_0 = inference_at(&events, 0);
    assert!(
        estimate(turn_0) >= threshold_tokens(),
        "the estimate crosses on turn 0: {turn_0}"
    );
    assert!(
        of_type(&events, "compaction_declined").is_empty(),
        "{events:#?}"
    );
    let compactions = of_type(&events, "compaction");
    assert_eq!(compactions.len(), 1, "{events:#?}");
    assert_eq!(compactions[0]["turn"], 1);
    assert_eq!(compactions[0]["tokens_before"], 520 + 60 + 20 + 20);
    assert_eq!(compactions[0]["tokens_before_source"], "actual");
}

/// A response whose cache members are what carries it over the threshold: each is counted once,
/// beside `input_tokens`.
#[test]
fn compaction_counts_each_cache_member_once() {
    if common::skip_without_host_support("compaction_counts_each_cache_member_once") {
        return;
    }
    let events = run_compacting(
        vec![
            response(
                Some("echo one"),
                Some(json!({"input_tokens": 50, "output_tokens": 10})),
            ),
            response(
                None,
                Some(json!({
                    "input_tokens": 50,
                    "cache_read_input_tokens": 200,
                    "cache_creation_input_tokens": 160,
                    "output_tokens": 10
                })),
            ),
        ],
        WINDOW,
        true,
        TASK,
    );

    // Neither cache member alone carries the turn over: 50 + 200 + 10 and 50 + 160 + 10 are
    // both under 400.
    assert!(50 + 200 + 10 < threshold_tokens() && 50 + 160 + 10 < threshold_tokens());
    let compactions = of_type(&events, "compaction");
    assert_eq!(compactions.len(), 1, "{events:#?}");
    assert_eq!(compactions[0]["turn"], 1);
    assert_eq!(compactions[0]["tokens_before"], 50 + 200 + 160 + 10);
    assert_eq!(compactions[0]["tokens_before_source"], "actual");
}

/// A driver that reports nothing leaves the estimate to decide, and the compaction says so.
#[test]
fn compaction_with_no_usage_fires_on_the_estimate() {
    if common::skip_without_host_support("compaction_with_no_usage_fires_on_the_estimate") {
        return;
    }
    let events = run_compacting(
        vec![response(Some("echo one"), None), response(None, None)],
        WINDOW,
        true,
        TASK,
    );

    let compactions = of_type(&events, "compaction");
    assert!(!compactions.is_empty(), "{events:#?}");
    assert_eq!(compactions[0]["turn"], 0);
    assert_eq!(compactions[0]["tokens_before_source"], "estimate");
    assert_eq!(
        compactions[0]["tokens_before"],
        estimate(inference_at(&events, 0))
    );
}

/// Messages added after a report are estimated on their own and added to it: turn 1, which
/// reported nothing, is measured as turn 0's reported request and completion plus the tool
/// result and assistant turn since, and not as the whole payload again.
#[test]
fn compaction_measures_messages_added_after_the_report() {
    if common::skip_without_host_support("compaction_measures_messages_added_after_the_report") {
        return;
    }
    // One cl100k token per ` word`, so the tool result alone is at least 300 tokens.
    const FLOOR: u64 = 300;
    let events = run_compacting(
        vec![
            response(
                Some("printf 'word %.0s' {1..300}"),
                Some(json!({"input_tokens": 10, "output_tokens": 5})),
            ),
            response(None, None),
        ],
        // Large enough that nothing here crosses, with no hook to fire.
        1_000_000,
        false,
        // Long enough that the whole first request outweighs what turn 0 adds. The report covers
        // it, so turn 1 does not count it again.
        &format!("{TASK}{}", " context".repeat(1_000)),
    );

    let tool_call = of_type(&events, "tool_call")
        .into_iter()
        .find(|e| e["turn"] == 0)
        .unwrap_or_else(|| panic!("turn 0 ran bash: {events:#?}"));
    assert!(
        tool_call["output_bytes"].as_u64().unwrap() >= FLOOR * 5,
        "the command printed its words: {tool_call}"
    );
    let whole = inference_at(&events, 0)["input_tokens"].as_u64().unwrap();
    let turn_1 = inference_at(&events, 1)["input_tokens"].as_u64().unwrap();
    assert!(turn_1 >= 10 + 5 + FLOOR, "turn 1 {turn_1}");
    assert!(
        turn_1 < whole,
        "turn 1 {turn_1} is anchored on the report, not the whole payload ({whole} on turn 0)"
    );
}
