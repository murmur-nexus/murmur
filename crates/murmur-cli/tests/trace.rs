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

/// Every non-null `parent_id` in file order must name an `event_id` written earlier in the
/// same file, and every `event_id` must be a distinct, well-formed `evt_` id. Panics with the
/// offending line when either fails — this is the invariant that makes the file walkable.
fn assert_walkable_tree(events: &[Value]) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, event) in events.iter().enumerate() {
        let event_id = event["event_id"]
            .as_str()
            .unwrap_or_else(|| panic!("event {i} has no event_id: {event}"));
        assert!(
            event_id.len() == 36
                && event_id.starts_with("evt_")
                && event_id[4..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "event {i} has a malformed event_id {event_id:?}"
        );
        assert!(
            seen.insert(event_id.to_string()),
            "event {i} reuses event_id {event_id:?}"
        );
        assert!(
            event.get("parent_id").is_some(),
            "event {i} has no parent_id key: {event}"
        );
        if let Some(parent) = event["parent_id"].as_str() {
            assert!(
                seen.contains(parent),
                "event {i} names parent_id {parent:?}, which appears nowhere earlier: {event}"
            );
        }
    }
}

fn find<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|e| e["event_type"] == event_type)
        .unwrap_or_else(|| panic!("trace must contain a {event_type} event"))
}

fn count_of(events: &[Value], event_type: &str) -> usize {
    events
        .iter()
        .filter(|e| e["event_type"] == event_type)
        .count()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Happy-path two-turn session: tool_call (bash) then end_turn.
/// Verifies all six event types appear, session_id is consistent, and counts match.
#[test]
fn trace_two_turn_session_all_event_types() {
    if common::skip_without_host_support("trace_two_turn_session_all_event_types") {
        return;
    }
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

    // ── Outer frame is the launch: one session_start first, one session_end last,
    //    with the task lifecycle nested inside. ──
    assert_eq!(
        events.first().unwrap()["event_type"],
        "session_start",
        "first event must be session_start"
    );
    assert_eq!(
        events.last().unwrap()["event_type"],
        "session_end",
        "last event must be session_end"
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

    // ── Every line is identified and parents to an earlier line ──
    assert_walkable_tree(&events);

    let session_node = ss["event_id"].as_str().unwrap();
    assert!(
        ss["parent_id"].is_null(),
        "session_start is the root and parents to nothing"
    );

    let ts = find(&events, "task_start");
    assert_eq!(
        ts["parent_id"], session_node,
        "task_start must parent to the session node"
    );
    let task_node = ts["event_id"].as_str().unwrap();

    let te = find(&events, "task_end");
    assert_eq!(
        te["parent_id"], task_node,
        "task_end must parent to its task_start"
    );
    assert_eq!(
        se["parent_id"], session_node,
        "session_end must parent to the session node"
    );

    // tool_call and shell hang off the inference line of their own turn.
    let tool = find(&events, "tool_call");
    let shell = find(&events, "shell");
    let turn_node = events
        .iter()
        .find(|e| {
            e["event_type"] == "inference" && e["turn"] == tool["turn"] && e["origin"].is_null()
        })
        .expect("the tool call's turn must have an agent-loop inference line");
    assert_eq!(
        tool["parent_id"], turn_node["event_id"],
        "tool_call must parent to its turn's inference"
    );
    assert_eq!(
        shell["parent_id"], turn_node["event_id"],
        "shell must parent to its turn's inference"
    );

    // ── Every turn-level line inside the task carries its task_id ──
    let task_id = ts["task_id"].as_str().unwrap();
    for event in &events {
        let ty = event["event_type"].as_str().unwrap();
        if matches!(
            ty,
            "inference" | "tool_call" | "shell" | "skill_call" | "compaction"
        ) {
            assert_eq!(
                event["task_id"], task_id,
                "{ty} written inside the task must carry the task's id: {event}"
            );
        }
    }

    // ── The provider's own id for the call is recorded verbatim ──
    assert_eq!(
        tool["tool_call_id"], "toolu_1",
        "tool_call must carry the provider's tool_call_id"
    );

    // ── Exactly one frame, with the task pair strictly inside it ──
    assert_eq!(count_of(&events, "session_start"), 1);
    assert_eq!(count_of(&events, "session_end"), 1);
    assert_eq!(count_of(&events, "task_start"), 1);
    assert_eq!(count_of(&events, "task_end"), 1);
    let index_of = |ty: &str| events.iter().position(|e| e["event_type"] == ty).unwrap();
    assert!(
        index_of("session_start") < index_of("task_start")
            && index_of("task_end") < index_of("session_end"),
        "the task pair must sit strictly inside the session frame"
    );
    assert_eq!(
        se["total_turns"].as_u64().unwrap() as usize,
        count_of(&events, "inference")
    );
    assert_eq!(
        se["total_tool_calls"].as_u64().unwrap() as usize,
        count_of(&events, "tool_call")
    );
    assert_eq!(
        se["total_shell_calls"].as_u64().unwrap() as usize,
        count_of(&events, "shell")
    );
}

/// Session without hook artifacts still produces trace.jsonl.
/// No murmur-hook-debug or any hook declared — only the driver.
#[test]
fn trace_written_without_hook_artifacts() {
    if common::skip_without_host_support("trace_written_without_hook_artifacts") {
        return;
    }
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
    assert_eq!(events.first().unwrap()["event_type"], "session_start");
    assert_eq!(events.last().unwrap()["event_type"], "session_end");
    assert_eq!(events.last().unwrap()["exit_status"], "ok");
    // exit_status is recorded on the task frame too.
    assert_eq!(find(&events, "task_end")["exit_status"], "ok");
    assert_walkable_tree(&events);
}

/// session_end is written even when the session exits with "failed" status.
/// Achieved by scripting only one response (tool_call) and letting the server
/// close so the second driver call fails → driver returns non-passed status.
#[test]
fn trace_session_end_written_on_failed_exit() {
    if common::skip_without_host_support("trace_session_end_written_on_failed_exit") {
        return;
    }
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
    assert_walkable_tree(&events);

    // One session frame per launch, closed on the failing exit path.
    assert_eq!(
        count_of(&events, "session_end"),
        1,
        "session_end must appear exactly once"
    );
    let se = events.last().unwrap();
    assert_eq!(
        se["event_type"], "session_end",
        "session_end must be the last event even on failure"
    );
    assert_eq!(
        se["exit_status"], "failed",
        "the launch's exit_status must be failed"
    );

    // task_end carries the attempt's own terminal status, not the launch's.
    assert_ne!(
        find(&events, "task_end")["exit_status"],
        "ok",
        "task_end must report the attempt's own failure, not a coarse ok"
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
    if common::skip_without_host_support("trace_session_id_matches_staged_session") {
        return;
    }
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

// ── Effective grant set on session_start ─────────────────────────────────────

/// A manifest whose grants are all *non-process* ones — network destinations, a filesystem scope
/// and an env allowlist — so the session launches on any host.
///
/// Deliberately no `shell.allow`/`spawn.allow`: either would make this capsule require a bounded
/// process tree, and a host whose systemd unit has not delegated the cgroup controllers refuses
/// the launch outright (`E-RUN-…`/`CgroupDelegationUnavailable`) before a trace is ever written.
/// Those two categories, and `interpreter_runtime` alongside them, are covered against the same
/// whole-object equality assertion by `trace::tests::
/// session_start_records_the_whole_scope_report_as_effective_grants` in `capsule-runtime`, which
/// needs no kernel at all.
fn create_grants_manifest(project_dir: &std::path::Path, endpoint: &str) -> PathBuf {
    let manifest = format!(
        concat!(
            "name: trace-grants-test\n",
            "version: 0.3.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "  containment: advisory\n",
            "  filesystem:\n",
            "    scope: ./workdir\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "      - https://api.example.com\n",
            "  env:\n",
            "    allow:\n",
            "      - MURMUR_TEST_REGION\n",
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

/// Headline property, end to end: `session_start.effective_grants` in a real
/// `trace.jsonl` equals, field for field, the object `mur run --explain-scope --json` prints for
/// the same manifest on the same host.
///
/// Both sides go through `containment::scope_report_for_tier` from the same manifest-derived
/// `CapabilityPolicy` and the same declared floor, so the only thing that could make them differ
/// is live host state changing between the two invocations — which does not happen inside one
/// test. Comparing the whole object, rather than a hand-listed set of keys, is what makes a field
/// later added to `ScopeReport` on only one of the two paths fail here.
#[test]
fn session_start_effective_grants_match_explain_scope_json() {
    let server = common::ScriptedServer::start(vec![end_turn_response()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    setup_driver(&home, &artifact_dir);

    let manifest_path = create_grants_manifest(project.path(), &server.endpoint);

    // The read-only diagnostic first: it stages nothing, so it cannot disturb the run below.
    let explain_stdout = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let explain_report: Value =
        serde_json::from_str(String::from_utf8(explain_stdout).unwrap().trim()).unwrap();

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Report and stop.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let events = parse_events(&workdir);
    let session_start = events
        .iter()
        .find(|e| e["event_type"] == "session_start")
        .expect("a completed session must have written session_start");

    assert_eq!(
        session_start["effective_grants"], explain_report,
        "session_start.effective_grants must be the ScopeReport --explain-scope --json prints"
    );

    // Spot-checks that the manifest's own declarations survived verbatim into the trace, so a
    // failure above reads as "these two disagree" rather than "both are empty and equal".
    let grants = &session_start["effective_grants"];
    assert_eq!(
        grants["network_allow"],
        json!([server.endpoint, "https://api.example.com"])
    );
    assert_eq!(grants["env_allow"], json!(["MURMUR_TEST_REGION"]));
    assert_eq!(grants["filesystem_scope"], json!("./workdir"));
    assert_eq!(grants["declared_containment"], json!("advisory"));
    assert!(
        grants["enforcement_tier"].is_string(),
        "the probed tier must be recorded as a wire name"
    );

    // The pre-existing summary fields are untouched by the new object sitting next to them, and
    // agree with it — they are now read off the same report.
    //
    // Note what `capabilities` does *not* say: this manifest's `env.allow` has no category name at
    // all, and `network` names no destination. That gap is the reason `effective_grants` exists.
    assert_eq!(
        session_start["capabilities"],
        json!(["network", "filesystem"])
    );
    assert_eq!(
        session_start["containment_declared"],
        grants["declared_containment"]
    );
    assert_eq!(
        session_start["containment_achieved"],
        grants["achieved_containment"]
    );
    assert_eq!(session_start["workdir_exec"], grants["workdir_exec"]);
}

/// Two manifests differing only in `capabilities.network.allow` produce `effective_grants` entries
/// that differ exactly as the manifests do. The whole point of recording the grant set is that
/// this is legible from the trace alone, with neither manifest in hand.
#[test]
fn effective_grants_network_allow_tracks_the_manifest_across_two_runs() {
    fn network_allow_for(capabilities_yaml: &str, endpoint: &str) -> Value {
        let home = tempfile::tempdir().unwrap();
        let artifact_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        setup_driver(&home, &artifact_dir);

        let manifest = format!(
            concat!(
                "name: trace-grants-diff\n",
                "version: 0.1.0\n",
                "artifacts:\n",
                "  - name: {driver_name}\n",
                "    version: {driver_version}\n",
                "    runtime: driver\n",
                "{capabilities}",
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
            capabilities = capabilities_yaml,
            endpoint = endpoint,
        );
        let manifest_path = project.path().join("murmur.yaml");
        fs::write(&manifest_path, manifest).unwrap();

        let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
        let workdir = staged.workdir.clone();
        fs::write(workdir.join("task.md"), "Report and stop.").unwrap();
        launch_session(staged, |_| {}).expect("agent launch should succeed");

        parse_events(&workdir)
            .into_iter()
            .find(|e| e["event_type"] == "session_start")
            .expect("session_start must be present")["effective_grants"]["network_allow"]
            .clone()
    }

    let server = common::ScriptedServer::start(vec![end_turn_response(), end_turn_response()]);

    let with_destinations = network_allow_for(
        &format!(
            "capabilities:\n  network:\n    allow:\n      - {}\n",
            server.endpoint
        ),
        &server.endpoint,
    );
    let without_destinations = network_allow_for("", &server.endpoint);

    assert_eq!(with_destinations, json!([server.endpoint]));
    assert_eq!(without_destinations, json!([]));
    assert_ne!(with_destinations, without_destinations);
}
