//! End-to-end coverage for the required-field check on `transport: http`: a call whose input
//! lacks a name its tool's `input_schema` lists in `required` is refused before the tool runs,
//! the model is told which fields were missing, and `trace.jsonl` carries a `tool_input_refused`
//! record beside the failed `tool_call`.
//!
//! The tool is a local native fixture named `murmur-tool-editor` whose schema mirrors the
//! editor's, so nothing here depends on a `default-artifacts` checkout.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use common::hook_wat::{create_hook_zip, deny_hook_wasm};
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

const EDITOR: &str = "murmur-tool-editor";

/// The editor's schema: `operation` and `dest_path` are required, `content` is not.
const EDITOR_SCHEMA: &str = r#"{"type":"object","properties":{"operation":{"type":"string","enum":["write_file","replace_in_file"]},"dest_path":{"type":"string"},"content":{"type":"string"}},"required":["operation","dest_path"]}"#;

/// The file the fixture tool writes whenever it runs. Its absence is the measurement.
const MARKER: &str = "tool-ran.txt";

/// The file the fixture tool writes as its "edit".
const NOTES: &str = "notes.txt";

/// A native tool that writes [`MARKER`] and [`NOTES`] into the workdir and answers `passed`.
fn editor_script() -> String {
    format!(
        "#!/bin/sh\n: > {MARKER}\necho hello > {NOTES}\necho '{}'\n",
        r#"{"status":"passed","summary":"ok","data":"EDITOR-RAN","data_path":null,"truncated":false,"metadata":[]}"#
    )
}

fn tool_call(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One capsule to run: its native tools with their schemas, an optional deny-all
/// `on-tool-call` hook, an optional turn budget, and whether it may submit plans.
#[derive(Default)]
struct Capsule<'a> {
    tools: Vec<(&'a str, Option<&'a str>)>,
    deny_hook: bool,
    max_turns: Option<u32>,
    plan_submit: bool,
}

impl<'a> Capsule<'a> {
    fn editor(schema: Option<&'a str>) -> Self {
        Self {
            tools: vec![(EDITOR, schema)],
            ..Self::default()
        }
    }
}

const HOOK_NAME: &str = "tool-policy";
const HOOK_REASON: &str = "editing is not allowed here";

fn create_manifest(project_dir: &Path, endpoint: &str, capsule: &Capsule<'_>) -> PathBuf {
    let hook_yaml = if capsule.deny_hook {
        format!("  - name: {HOOK_NAME}\n    version: 0.1.0\n    runtime: hook\n")
    } else {
        String::new()
    };
    let tool_yaml: String = capsule
        .tools
        .iter()
        .map(|(name, _)| format!("  - name: {name}\n    version: 0.1.0\n    runtime: tool\n"))
        .collect();
    let plan = if capsule.plan_submit {
        "  plan:\n    submit: true\n"
    } else {
        ""
    };
    let max_turns = capsule
        .max_turns
        .map(|turns| format!("  max_turns: {turns}\n"))
        .unwrap_or_default();

    let manifest = format!(
        concat!(
            "name: required-fields-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "    gateway:\n",
            "      endpoint: {endpoint}\n",
            "      api_key: test-key\n",
            "{hook_yaml}",
            "{tool_yaml}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "{plan}",
            "inference:\n",
            "  transport: http\n",
            "  model: test-model\n",
            "{max_turns}",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        hook_yaml = hook_yaml,
        tool_yaml = tool_yaml,
        endpoint = endpoint,
        plan = plan,
        max_turns = max_turns,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// What one launched session left behind.
struct Session {
    /// `launch_session`'s error, rendered, for a session that ended on anything but `ok`.
    launch_error: Option<String>,
    requests: Vec<Value>,
    trace: Vec<Value>,
    trace_raw: String,
    workdir: PathBuf,
    _project: tempfile::TempDir,
}

impl Session {
    fn events(&self, event_type: &str) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|e| e["event_type"] == event_type)
            .collect()
    }

    fn tool_calls(&self, tool_name: &str) -> Vec<&Value> {
        self.events("tool_call")
            .into_iter()
            .filter(|e| e["tool_name"] == tool_name)
            .collect()
    }

    /// The one `tool_input_refused` line, with the whole trace in the failure message.
    fn refusal(&self) -> &Value {
        let refusals = self.events("tool_input_refused");
        assert_eq!(
            refusals.len(),
            1,
            "expected exactly one tool_input_refused line; workdir {}; trace was:\n{}",
            self.workdir.display(),
            self.trace_raw
        );
        refusals[0]
    }

    fn ran(&self) -> bool {
        self.workdir.join(MARKER).exists()
    }

    fn wrote_notes(&self) -> bool {
        self.workdir.join(NOTES).exists()
    }

    /// The text of the tool result the driver was handed for `tool_use_id`.
    fn tool_result_text(&self, tool_use_id: &str) -> String {
        let block = common::find_tool_result(&self.requests, tool_use_id)
            .unwrap_or_else(|| panic!("no tool_result for {tool_use_id}"));
        common::extract_result_text(&block)
    }

    /// The session's `exit_status`; a session that ended on anything but `ok` must also have
    /// returned an error from `launch_session`.
    fn exit_status(&self) -> &str {
        let ends = self.events("session_end");
        assert_eq!(ends.len(), 1, "trace was:\n{}", self.trace_raw);
        let status = ends[0]["exit_status"].as_str().unwrap_or_default();
        assert_eq!(
            status == "ok",
            self.launch_error.is_none(),
            "exit_status {status} against launch error {:?}",
            self.launch_error
        );
        status
    }

    fn assert_no_policy_refusal(&self) {
        for record in ["call_denied", "protected_path_denied"] {
            assert!(
                self.events(record).is_empty(),
                "no {record} beside a required-field refusal:\n{}",
                self.trace_raw
            );
        }
    }
}

/// Publish the driver, the hook if any and every native tool, stage the capsule, run it, and
/// collect what it left.
fn run_session(responses: Vec<String>, capsule: &Capsule<'_>) -> Session {
    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    if capsule.deny_hook {
        let artifact = create_hook_zip(
            artifact_dir.path(),
            HOOK_NAME,
            "on-tool-call",
            "deny",
            &deny_hook_wasm("on-tool-call", HOOK_REASON),
        );
        common::publish_local(&home, &artifact).success();
    }

    let script = editor_script();
    for (name, schema) in &capsule.tools {
        let artifact = common::create_native_artifact(
            artifact_dir.path(),
            name,
            "0.1.0",
            &script,
            Some("Fixture editor"),
            *schema,
        );
        common::publish_local(&home, &artifact).success();
    }

    let manifest_path = create_manifest(project.path(), &server.endpoint, capsule);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Edit the notes.").unwrap();

    let launch_error = launch_session(staged, |_| {})
        .err()
        .map(|e| format!("{e:?}"));

    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        launch_error,
        requests: server.requests(),
        trace,
        trace_raw,
        workdir,
        _project: project,
    }
}

/// One editor call with `input`, then an `end_turn`.
fn one_editor_call(input: Value) -> Vec<String> {
    vec![
        tool_call("msg_1", "toolu_edit", EDITOR, input),
        end_turn("msg_2", "Done."),
    ]
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// S1: a call missing `operation` never reaches the tool; the model is told which field and that
/// only presence was checked; the trace holds one `tool_input_refused` beside one failed
/// `tool_call`, and the session ends normally.
#[test]
fn http_call_missing_operation_is_refused() {
    if common::skip_without_host_support("http_call_missing_operation_is_refused") {
        return;
    }
    let session = run_session(
        one_editor_call(json!({"dest_path": NOTES, "content": "hello"})),
        &Capsule::editor(Some(EDITOR_SCHEMA)),
    );

    assert!(!session.ran(), "the refused tool must not run");
    assert!(!session.wrote_notes(), "the refused call must not write");

    let text = session.tool_result_text("toolu_edit");
    assert!(
        text.starts_with(r#"murmur-tool-editor: missing required field "operation""#),
        "{text}"
    );
    assert!(
        text.contains("Only the presence of required fields is checked"),
        "{text}"
    );

    let refusal = session.refusal();
    assert_eq!(refusal["tool_name"], EDITOR);
    assert_eq!(refusal["tool_call_id"], "toolu_edit");
    assert_eq!(refusal["missing"], json!(["operation"]));
    assert_eq!(refusal["reason"], text.as_str());

    // The runtime marks the tool message `is_error: true`; the Anthropic driver fixture does not
    // carry that flag onto the wire block, so the failed status is read off the trace.
    let calls = session.tool_calls(EDITOR);
    assert_eq!(calls.len(), 1, "trace was:\n{}", session.trace_raw);
    assert_eq!(calls[0]["status"], "error");
    assert_eq!(calls[0]["tool_call_id"], "toolu_edit");

    session.assert_no_policy_refusal();
    assert_eq!(session.exit_status(), "ok");
}

/// S3: every missing field is named, in the schema's order.
#[test]
fn every_missing_field_is_named() {
    if common::skip_without_host_support("every_missing_field_is_named") {
        return;
    }
    let session = run_session(
        one_editor_call(json!({"content": "hello"})),
        &Capsule::editor(Some(EDITOR_SCHEMA)),
    );

    assert!(!session.ran());
    let text = session.tool_result_text("toolu_edit");
    assert!(
        text.starts_with(r#"murmur-tool-editor: missing required fields "operation", "dest_path""#),
        "{text}"
    );
    assert_eq!(
        session.refusal()["missing"],
        json!(["operation", "dest_path"])
    );
}

/// S4 (http): a call carrying every required field runs exactly as before.
#[test]
fn a_complete_call_runs() {
    if common::skip_without_host_support("a_complete_call_runs") {
        return;
    }
    let session = run_session(
        one_editor_call(json!({
            "operation": "write_file",
            "dest_path": NOTES,
            "content": "hello"
        })),
        &Capsule::editor(Some(EDITOR_SCHEMA)),
    );

    assert!(session.ran(), "trace was:\n{}", session.trace_raw);
    assert!(session.events("tool_input_refused").is_empty());
    let calls = session.tool_calls(EDITOR);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "ok");
    assert!(session
        .tool_result_text("toolu_edit")
        .contains("EDITOR-RAN"));
}

/// S5: a tool that declares no `input_schema` is not checked, whatever it is called with.
#[test]
fn a_tool_without_required_is_unaffected() {
    if common::skip_without_host_support("a_tool_without_required_is_unaffected") {
        return;
    }
    let session = run_session(one_editor_call(json!({})), &Capsule::editor(None));

    assert!(session.ran(), "trace was:\n{}", session.trace_raw);
    assert!(session.events("tool_input_refused").is_empty());
    assert_eq!(session.tool_calls(EDITOR)[0]["status"], "ok");
}

/// S6: a `required` that is not an array of strings is not enforced — both calls run.
#[test]
fn a_malformed_schema_is_not_enforced() {
    if common::skip_without_host_support("a_malformed_schema_is_not_enforced") {
        return;
    }
    let session = run_session(
        vec![
            tool_call("msg_1", "toolu_a", EDITOR, json!({"dest_path": NOTES})),
            tool_call("msg_2", "toolu_b", EDITOR, json!({"dest_path": NOTES})),
            end_turn("msg_3", "Done."),
        ],
        &Capsule::editor(Some(
            r#"{"type":"object","properties":{"operation":{"type":"string"}},"required":"operation"}"#,
        )),
    );

    assert!(session.ran(), "trace was:\n{}", session.trace_raw);
    assert!(session.events("tool_input_refused").is_empty());
    let calls = session.tool_calls(EDITOR);
    assert_eq!(calls.len(), 2, "trace was:\n{}", session.trace_raw);
    assert!(calls.iter().all(|call| call["status"] == "ok"));
}

/// S7: a refusal is an ordinary tool result — the next turn sees it, and a model that keeps
/// retrying spends `inference.max_turns` like any other turn.
#[test]
fn repeated_refusals_spend_the_turn_budget() {
    if common::skip_without_host_support("repeated_refusals_spend_the_turn_budget") {
        return;
    }
    let bad = json!({"dest_path": NOTES, "content": "hello"});
    let session = run_session(
        vec![
            tool_call("msg_1", "toolu_a", EDITOR, bad.clone()),
            tool_call("msg_2", "toolu_b", EDITOR, bad),
            end_turn("msg_3", "never reached"),
        ],
        &Capsule {
            max_turns: Some(2),
            ..Capsule::editor(Some(EDITOR_SCHEMA))
        },
    );

    assert!(!session.ran(), "a refused call never reaches the tool");
    assert_eq!(
        session.requests.len(),
        2,
        "the budget allows exactly two inference calls"
    );
    // The second request carries the first refusal as a tool result.
    let first = common::find_tool_result(&session.requests[1..], "toolu_a")
        .expect("the second request carries the first refusal");
    assert!(common::extract_result_text(&first)
        .starts_with(r#"murmur-tool-editor: missing required field "operation""#));

    assert_eq!(
        session.events("tool_input_refused").len(),
        2,
        "trace was:\n{}",
        session.trace_raw
    );
    assert_eq!(session.exit_status(), "max_turns_reached");
    assert!(
        session
            .launch_error
            .as_deref()
            .is_some_and(|e| e.contains("TaskDidNotComplete")),
        "the run ends on the turn budget's own ending: {:?}",
        session.launch_error
    );
}

/// S8: the required-field check runs before a policy hook. A call missing a field gets the
/// required-field text and the hook is not asked; a complete call is denied by the hook.
#[test]
fn the_required_check_runs_before_a_policy_hook() {
    if common::skip_without_host_support("the_required_check_runs_before_a_policy_hook") {
        return;
    }
    let session = run_session(
        vec![
            tool_call(
                "msg_1",
                "toolu_missing",
                EDITOR,
                json!({"dest_path": NOTES, "content": "hello"}),
            ),
            tool_call(
                "msg_2",
                "toolu_complete",
                EDITOR,
                json!({"operation": "write_file", "dest_path": NOTES, "content": "hello"}),
            ),
            end_turn("msg_3", "Done."),
        ],
        &Capsule {
            deny_hook: true,
            ..Capsule::editor(Some(EDITOR_SCHEMA))
        },
    );

    assert!(!session.ran());

    let missing = session.tool_result_text("toolu_missing");
    assert!(
        missing.starts_with(r#"murmur-tool-editor: missing required field "operation""#),
        "{missing}"
    );
    assert!(!missing.contains(HOOK_NAME), "{missing}");
    assert_eq!(session.refusal()["tool_call_id"], "toolu_missing");

    let complete = session.tool_result_text("toolu_complete");
    assert!(complete.contains(HOOK_NAME), "{complete}");
    assert!(complete.contains(HOOK_REASON), "{complete}");

    let denials = session.events("call_denied");
    assert_eq!(
        denials.len(),
        1,
        "only the complete call reaches the hook:\n{}",
        session.trace_raw
    );
    assert_eq!(denials[0]["hook_name"], HOOK_NAME);
    assert_eq!(denials[0]["target"], EDITOR);
}

/// S9: a plan's `tool` step missing a required field fails without running, and its `plan_step`
/// record's `error` carries the refusal text.
#[test]
fn a_plan_step_missing_a_required_field_fails() {
    if common::skip_without_host_support("a_plan_step_missing_a_required_field_fails") {
        return;
    }
    let plan = json!({
        "id": "edit-notes",
        "steps": [{
            "id": "edit",
            "tool": EDITOR,
            "input": {"dest_path": NOTES, "content": "hello"}
        }]
    });
    let session = run_session(
        vec![
            tool_call("msg_1", "toolu_plan", "submit-plan", json!({"plan": plan})),
            end_turn("msg_2", "Done."),
        ],
        &Capsule {
            plan_submit: true,
            ..Capsule::editor(Some(EDITOR_SCHEMA))
        },
    );

    assert!(!session.ran(), "trace was:\n{}", session.trace_raw);
    let steps = session.events("plan_step");
    assert_eq!(steps.len(), 1, "trace was:\n{}", session.trace_raw);
    assert_eq!(steps[0]["step_id"], "edit");
    assert_eq!(steps[0]["status"], "failed", "{}", steps[0]);
    let error = steps[0]["error"].as_str().unwrap_or_default();
    assert!(
        error.starts_with(r#"murmur-tool-editor: missing required field "operation""#),
        "{error}"
    );
    // The plan step's own record carries the reason, so the session writes no separate one.
    assert!(session.events("tool_input_refused").is_empty());
}
