#[path = "common/mod.rs"]
mod common;

use std::{fs, path::PathBuf};

use capsule_runtime::launch_session;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.7";

// ── Response builders ────────────────────────────────────────────────────────

fn tool_call_response(tool_name: &str, input: Value) -> String {
    json!({
        "id": "msg_tc",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": "toolu_1",
            "name": tool_name,
            "input": input,
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_et",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 8, "output_tokens": 3},
    })
    .to_string()
}

// ── Manifest helper ──────────────────────────────────────────────────────────

fn create_manifest(project_dir: &std::path::Path, endpoint: &str) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    let manifest = format!(
        concat!(
            "name: trace-test\n",
            "version: 0.2.0\n",
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
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

fn setup_driver(home: &tempfile::TempDir, artifact_dir: &tempfile::TempDir) {
    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver_artifact).success();
}

fn parse_events(workdir: &std::path::Path) -> Vec<Value> {
    let content = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Happy-path two-turn session: tool_call (bash) then end_turn.
/// Verifies all six event types appear, session_id is consistent, and counts match.
#[test]
fn trace_two_turn_session_all_event_types() {
    let server = common::ScriptedServer::start(vec![
        tool_call_response("bash", json!({"command": "echo hello"})),
        end_turn_response(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    setup_driver(&home, &artifact_dir);

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    // trace.jsonl must exist
    assert!(
        workdir.join("trace.jsonl").exists(),
        "trace.jsonl must exist after session"
    );

    let events = parse_events(&workdir);
    assert!(!events.is_empty(), "trace.jsonl must not be empty");

    // ── Outer frame is the task lifecycle: task_start first, task_end last.
    //    The session lifecycle (session_start/session_end) is nested inside. ──
    assert_eq!(
        events.first().unwrap()["event_type"],
        "task_start",
        "first event must be task_start"
    );
    assert_eq!(
        events.last().unwrap()["event_type"],
        "task_end",
        "last event must be task_end"
    );

    // ── All event types present ──
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"task_start"), "must contain task_start");
    assert!(
        types.contains(&"session_start"),
        "must contain session_start"
    );
    assert!(types.contains(&"inference"), "must contain inference");
    assert!(types.contains(&"tool_call"), "must contain tool_call");
    assert!(types.contains(&"shell"), "must contain shell");
    assert!(types.contains(&"session_end"), "must contain session_end");
    assert!(types.contains(&"task_end"), "must contain task_end");

    // ── session_start and session_end appear exactly once ──
    assert_eq!(
        types.iter().filter(|&&t| t == "session_start").count(),
        1,
        "session_start must appear exactly once"
    );
    assert_eq!(
        types.iter().filter(|&&t| t == "session_end").count(),
        1,
        "session_end must appear exactly once"
    );

    // ── session_id is consistent across all events ──
    let session_id = events[0]["session_id"].as_str().unwrap();
    assert!(
        session_id.starts_with("ses_"),
        "session_id must start with ses_"
    );
    assert_eq!(
        session_id.len(),
        36,
        "session_id must be 36 chars (ses_ + 32 hex)"
    );
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event["session_id"].as_str().unwrap(),
            session_id,
            "session_id must be identical on all events (event {i})"
        );
    }

    // ── session_start fields (nested inside the task frame) ──
    let ss = events
        .iter()
        .find(|e| e["event_type"] == "session_start")
        .unwrap();
    assert_eq!(ss["model"], "test-model");
    assert_eq!(ss["max_turns"], 10_u64);
    assert!(
        ss["capsule_name"].as_str().is_some(),
        "capsule_name required"
    );
    assert!(
        ss["tools_declared"].as_array().is_some(),
        "tools_declared must be an array"
    );
    assert!(ss["timestamp"].as_u64().unwrap() > 0);

    // ── session_end fields ──
    let se = events
        .iter()
        .find(|e| e["event_type"] == "session_end")
        .unwrap();
    assert_eq!(se["exit_status"], "ok");
    assert!(se["duration_ms"].as_u64().is_some());

    // ── Count consistency ──
    let inference_count = types.iter().filter(|&&t| t == "inference").count() as u64;
    let tool_count = types.iter().filter(|&&t| t == "tool_call").count() as u64;
    let shell_count = types.iter().filter(|&&t| t == "shell").count() as u64;

    assert_eq!(
        se["total_turns"].as_u64().unwrap(),
        inference_count,
        "total_turns must equal inference event count"
    );
    assert_eq!(
        se["total_tool_calls"].as_u64().unwrap(),
        tool_count,
        "total_tool_calls must equal tool_call event count"
    );
    assert_eq!(
        se["total_shell_calls"].as_u64().unwrap(),
        shell_count,
        "total_shell_calls must equal shell event count"
    );

    // ── Field name format: snake_case (not camelCase) ──
    let inference_event = events
        .iter()
        .find(|e| e["event_type"] == "inference")
        .unwrap();
    assert!(
        inference_event.get("input_tokens").is_some(),
        "must have input_tokens"
    );
    assert!(
        inference_event.get("output_tokens").is_some(),
        "must have output_tokens"
    );
    assert!(
        inference_event.get("inputTokens").is_none(),
        "must NOT have camelCase inputTokens"
    );
    assert!(
        inference_event.get("toolName").is_none(),
        "must NOT have camelCase toolName"
    );

    let tool_event = events
        .iter()
        .find(|e| e["event_type"] == "tool_call")
        .unwrap();
    assert!(tool_event.get("tool_name").is_some(), "must have tool_name");
    assert!(
        tool_event.get("input_bytes").is_some(),
        "must have input_bytes"
    );
    assert!(
        tool_event.get("output_bytes").is_some(),
        "must have output_bytes"
    );
    assert!(
        tool_event.get("duration_ms").is_some(),
        "must have duration_ms"
    );

    let shell_event = events.iter().find(|e| e["event_type"] == "shell").unwrap();
    assert!(
        shell_event.get("exit_code").is_some(),
        "must have exit_code"
    );
    assert!(
        shell_event.get("stdout_bytes").is_some(),
        "must have stdout_bytes"
    );
    assert!(
        shell_event.get("stderr_bytes").is_some(),
        "must have stderr_bytes"
    );

    assert!(
        se.get("total_input_tokens").is_some(),
        "must have total_input_tokens"
    );
    assert!(
        se.get("total_output_tokens").is_some(),
        "must have total_output_tokens"
    );
    assert!(se.get("exit_status").is_some(), "must have exit_status");
    assert!(
        se.get("exitStatus").is_none(),
        "must NOT have camelCase exitStatus"
    );
    assert!(
        se.get("totalTurns").is_none(),
        "must NOT have camelCase totalTurns"
    );
}

/// Session without hook artifacts still produces trace.jsonl.
/// No murmur-hook-debug or any hook declared — only the driver.
#[test]
fn trace_written_without_hook_artifacts() {
    let server = common::ScriptedServer::start(vec![end_turn_response()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    setup_driver(&home, &artifact_dir);

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Just say done.").unwrap();

    launch_session(staged, |_| {}).expect("should succeed");

    assert!(
        workdir.join("trace.jsonl").exists(),
        "trace.jsonl must exist even with no hook artifacts"
    );

    let events = parse_events(&workdir);
    assert!(!events.is_empty(), "trace must not be empty");
    assert_eq!(events.first().unwrap()["event_type"], "task_start");
    assert_eq!(events.last().unwrap()["event_type"], "task_end");
    // exit_status is recorded on the task_end frame too.
    assert_eq!(events.last().unwrap()["exit_status"], "ok");
}

/// session_end is written even when the session exits with "failed" status.
/// Achieved by scripting only one response (tool_call) and letting the server
/// close so the second driver call fails → driver returns non-passed status.
#[test]
fn trace_session_end_written_on_failed_exit() {
    // Script one successful response (tool_call turn 0).
    // The server thread exits after serving one response; turn 1's driver call
    // will fail to connect, causing the driver WASM to return a non-passed
    // ToolResult → agent writes session_end with exit_status "failed".
    let server = common::ScriptedServer::start(vec![tool_call_response(
        "bash",
        json!({"command": "echo hi"}),
    )]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    setup_driver(&home, &artifact_dir);

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Echo something.").unwrap();

    // Session may Ok or Err depending on how the driver reports the network failure.
    let _ = launch_session(staged, |_| {});

    assert!(
        workdir.join("trace.jsonl").exists(),
        "trace.jsonl must exist even after failed session"
    );

    let events = parse_events(&workdir);
    // task_start must be first
    assert_eq!(
        events.first().unwrap()["event_type"],
        "task_start",
        "task_start must be first event"
    );
    // task_end must be last (regardless of how the failure manifests)
    assert_eq!(
        events.last().unwrap()["event_type"],
        "task_end",
        "task_end must be last event even on failure"
    );

    // The session_end frame records the non-ok exit_status and the totals.
    let se = events
        .iter()
        .find(|e| e["event_type"] == "session_end")
        .unwrap();
    let exit_status = se["exit_status"].as_str().unwrap();
    assert_ne!(
        exit_status, "ok",
        "exit_status must not be ok on failed session"
    );

    // Count consistency still holds
    let types: Vec<&str> = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect();
    let inference_count = types.iter().filter(|&&t| t == "inference").count() as u64;
    let tool_count = types.iter().filter(|&&t| t == "tool_call").count() as u64;
    let shell_count = types.iter().filter(|&&t| t == "shell").count() as u64;

    assert_eq!(se["total_tool_calls"].as_u64().unwrap(), tool_count);
    assert_eq!(se["total_shell_calls"].as_u64().unwrap(), shell_count);
    assert_eq!(se["total_turns"].as_u64().unwrap(), inference_count);
}

/// Verify that session_id in trace.jsonl equals the session_id from StagedSession.
#[test]
fn trace_session_id_matches_staged_session() {
    let server = common::ScriptedServer::start(vec![end_turn_response()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    setup_driver(&home, &artifact_dir);

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    let expected_session_id = staged.session_id.clone();
    fs::write(workdir.join("task.md"), "done").unwrap();

    launch_session(staged, |_| {}).expect("should succeed");

    let events = parse_events(&workdir);
    for event in &events {
        assert_eq!(
            event["session_id"].as_str().unwrap(),
            expected_session_id,
            "session_id in trace must match StagedSession.session_id"
        );
    }
}
