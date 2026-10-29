//! A turn the provider cut at `inference.max_tokens` ends that turn, not the task.
//!
//! Driven through the real `mur` binary against the anthropic driver fixture and a scripted
//! provider, because the properties are what the provider was sent after a capped turn and what
//! the trace, the conversation record and `out/result.txt` say afterwards.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Write the report.";
const CONTEXT_ID: &str = "ctx_output_cap";
const CAPSULE: &str = "output-cap";
/// The cap every capsule here declares, and the value every surface names.
const CAP: u64 = 1_000;
const KEEP_WORKING: &str = "Keep working on the task. Check your work against what was asked.";

/// The fact line after a capped turn with no tool call.
fn no_call_fact() -> String {
    format!(
        "The previous turn reached the inference.max_tokens output cap of {CAP} tokens and was \
         cut off. It made no tool call."
    )
}

// ── Scripted provider responses ───────────────────────────────────────────────

fn response(content: Value, stop_reason: &str) -> String {
    json!({
        "id": "msg",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": stop_reason,
    })
    .to_string()
}

fn end_turn(text: &str) -> String {
    response(json!([{"type": "text", "text": text}]), "end_turn")
}

/// A turn cut at the cap with nothing in it: no text and no tool call.
fn capped_empty() -> String {
    response(json!([]), "max_tokens")
}

fn capped_text(text: &str) -> String {
    response(json!([{"type": "text", "text": text}]), "max_tokens")
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the anthropic driver fixture published and compiled.
fn home_with_driver() -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        &format!(
            "{}{}",
            common::FIXTURE_DRIVER_AUTH,
            common::FIXTURE_DRIVER_MODELS
        ),
    );
    common::publish_local(&home, &artifact).success();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .arg("precompile")
        .arg(&artifact)
        .assert()
        .success();
    home
}

/// An http capsule on `endpoint` declaring `inference.max_tokens: CAP`; `lifecycle` and
/// `inference` are spliced into their blocks.
fn http_manifest(endpoint: &str, lifecycle: &str, inference: &str) -> String {
    format!(
        "name: {CAPSULE}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         lifecycle:\n  task_acceptance: single\n{lifecycle}inference:\n  transport: http\n  \
         model: test-model\n  max_tokens: {CAP}\n{inference}  driver:\n    artifact: {DRIVER}\n"
    )
}

struct Run {
    home: PathBuf,
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout)
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(&self.records_dir().join("trace.jsonl"))
    }

    fn result(&self) -> String {
        fs::read_to_string(self.records_dir().join("out").join("result.txt")).unwrap_or_default()
    }

    /// The conversation record's messages for [`CONTEXT_ID`], in order.
    fn recorded(&self) -> Vec<Value> {
        let path = self
            .home
            .join(".murmur/conversations")
            .join(CAPSULE)
            .join(CONTEXT_ID)
            .join("conversation.jsonl");
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line.get("role").is_some())
            .collect()
    }

    /// Every stderr line naming `W-RUN-001`.
    fn warnings(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains("warning[W-RUN-001]"))
            .collect()
    }
}

/// `mur run --task … --context CONTEXT_ID --verbose` on the capsule `manifest_yaml`.
fn run(home: &TempDir, manifest_yaml: &str) -> Run {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(&manifest, manifest_yaml).unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MURMUR_A2A_TIMEOUT_SECS", "1")
        .current_dir(dir.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            TASK,
            "--context",
            CONTEXT_ID,
            "--verbose",
        ])
        .timeout(Duration::from_secs(300))
        .output()
        .unwrap();
    Run {
        home: home.path().to_path_buf(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        output,
    }
}

// ── Reading the trace and the requests ────────────────────────────────────────

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn only<'a>(trace: &'a [Value], event_type: &str) -> &'a Value {
    let found = events(trace, event_type);
    assert_eq!(found.len(), 1, "one {event_type} in {trace:#?}");
    found[0]
}

fn positions(trace: &[Value], event_type: &str) -> Vec<usize> {
    trace
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event_type"] == event_type)
        .map(|(index, _)| index)
        .collect()
}

fn decisions(trace: &[Value]) -> Vec<(String, String)> {
    events(trace, "inference")
        .iter()
        .map(|event| {
            (
                event["decision"].as_str().unwrap_or_default().to_string(),
                event["stop_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn pair(decision: &str, stop_reason: &str) -> (String, String) {
    (decision.to_string(), stop_reason.to_string())
}

fn messages(request: &Value) -> &Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("no messages in {request}"))
}

/// The one text of a user message whose content is a single text block. The driver marks the
/// last block for prompt caching, which is its own key, not text.
fn sole_text(message: &Value) -> &str {
    let blocks = message["content"]
        .as_array()
        .unwrap_or_else(|| panic!("no content in {message}"));
    assert_eq!(blocks.len(), 1, "{message}");
    assert_eq!(blocks[0]["type"], "text", "{message}");
    blocks[0]["text"].as_str().unwrap()
}

fn roles(request: &Value) -> Vec<&str> {
    messages(request)
        .iter()
        .map(|message| message["role"].as_str().unwrap_or_default())
        .collect()
}

// ── S2: an empty capped turn continues the task ───────────────────────────────

/// A capped turn with no text and no tool call is followed by exactly one user message stating
/// the fact, and no empty assistant message is sent; the task ends on the next real turn end.
#[test]
fn an_empty_capped_turn_continues_with_the_fact_message() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![capped_empty(), end_turn("done")]);
    let run = run(&home, &http_manifest(&server.endpoint, "", ""));
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(roles(&requests[0]), ["user"]);
    assert_eq!(roles(&requests[1]), ["user", "user"], "{}", requests[1]);
    assert_eq!(sole_text(&messages(&requests[1])[1]), no_call_fact());

    let trace = run.trace();
    assert_eq!(
        decisions(&trace),
        [pair("capped", "max_tokens"), pair("end_turn", "end_turn")]
    );
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(task_end["capped_turns"], 1);
    assert_eq!(task_end["turns"], 2);
    assert_eq!(run.result(), "done");

    let warnings = run.warnings();
    assert_eq!(warnings.len(), 1, "{}", run.context());
    assert!(
        warnings[0].contains(&format!(
            "this turn stopped at the inference.max_tokens output cap of {CAP} tokens and was \
             cut off; the task goes on with another turn"
        )),
        "{}",
        warnings[0]
    );

    // The record equals what was replayed: the task, the fact, the answer — no empty assistant.
    let recorded = run.recorded();
    let recorded_roles: Vec<&str> = recorded
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        recorded_roles,
        ["user", "user", "assistant"],
        "{recorded:#?}"
    );
    assert_eq!(sole_text(&recorded[1]), no_call_fact());
}

// ── S3: a capped turn with text continues, and its fragment is kept ──────────

/// The fragment goes into the context and the record with its `truncated` mark, the fact
/// follows it, and the fragment never reaches `out/result.txt`.
#[test]
fn a_capped_fragment_is_kept_marked_and_never_the_result() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        capped_text("The report covers, first, the"),
        end_turn("done"),
    ]);
    let run = run(&home, &http_manifest(&server.endpoint, "", ""));
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(roles(&requests[1]), ["user", "assistant", "user"]);
    let assistant = &messages(&requests[1])[1];
    assert_eq!(
        assistant["content"][0]["text"], "The report covers, first, the",
        "{assistant}"
    );
    assert_eq!(sole_text(&messages(&requests[1])[2]), no_call_fact());

    let recorded = run.recorded();
    let fragment = recorded
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(fragment["truncated"], true, "{fragment}");
    assert_eq!(
        fragment["content"],
        json!([{"type": "text", "text": "The report covers, first, the"}])
    );
    let answer = recorded.last().unwrap();
    assert_eq!(answer["role"], "assistant");
    assert!(answer.get("truncated").is_none(), "{answer}");

    let trace = run.trace();
    assert_eq!(only(&trace, "task_end")["capped_turns"], 1);
    assert_eq!(run.result(), "done");
}

// ── S4: a cut tool call is dropped unrun, and the fact names it ──────────────

/// A `tool_call` in a capped response is never dispatched or answered; the assistant message
/// keeps only its text, and the fact names the call.
#[test]
fn a_cut_tool_call_is_not_run_and_the_fact_names_it() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        response(
            json!([
                {"type": "text", "text": "Checking the session."},
                {"type": "tool_use", "id": "toolu_cut", "name": "session-status", "input": {}},
            ]),
            "max_tokens",
        ),
        end_turn("done"),
    ]);
    let run = run(&home, &http_manifest(&server.endpoint, "", ""));
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(roles(&requests[1]), ["user", "assistant", "user"]);
    let assistant = &messages(&requests[1])[1];
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "{assistant}");
    assert_eq!(blocks[0]["type"], "text", "{assistant}");
    assert!(
        !requests[1].to_string().contains("toolu_cut"),
        "neither the call nor a result for it is sent: {}",
        requests[1]
    );
    assert_eq!(
        sole_text(&messages(&requests[1])[2]),
        format!(
            "The previous turn reached the inference.max_tokens output cap of {CAP} tokens and \
             was cut off. Its tool call session-status was cut off with it and was not run."
        )
    );

    let trace = run.trace();
    assert!(events(&trace, "tool_call").is_empty(), "{trace:#?}");
    assert!(events(&trace, "session_status").is_empty(), "{trace:#?}");
    assert_eq!(
        decisions(&trace),
        [pair("capped", "max_tokens"), pair("end_turn", "end_turn")]
    );
}

// ── S5: two empty capped turns in a row end the task failed ──────────────────

#[test]
fn two_empty_capped_turns_in_a_row_end_the_task_failed() {
    let home = home_with_driver();
    let server =
        common::ScriptedServer::start(vec![capped_empty(), capped_empty(), end_turn("never sent")]);
    let run = run(&home, &http_manifest(&server.endpoint, "", ""));
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 2, "{}", run.context());
    let trace = run.trace();
    assert_eq!(
        decisions(&trace),
        [pair("capped", "max_tokens"), pair("capped", "max_tokens")]
    );
    let failed = only(&trace, "task_failed");
    assert_eq!(failed["cause"], "output_cap");
    let reason = format!(
        "two turns in a row (turn 0 and turn 1) stopped at the inference.max_tokens output cap \
         of {CAP} tokens with no text and no tool call"
    );
    assert_eq!(failed["reason"], reason.as_str());
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "failed");
    assert_eq!(task_end["capped_turns"], 2);
    assert_eq!(run.result(), format!("error: {reason}"));

    let warnings = run.warnings();
    assert_eq!(warnings.len(), 2, "{}", run.context());
    assert!(
        warnings[1].contains(
            "with no text and no tool call, the second in a row, so the task ends failed"
        ),
        "{}",
        warnings[1]
    );
}

// ── S7: the spend ceiling bounds a capped task ───────────────────────────────

/// `inference.max_session_tokens` refuses the call after the capped turn: one request, and the
/// refusal is the spend ceiling's own stop.
#[test]
fn the_spend_ceiling_refuses_the_call_after_a_capped_turn() {
    let home = home_with_driver();
    let control_server = common::ScriptedServer::start(vec![end_turn("one")]);
    let control = run(&home, &http_manifest(&control_server.endpoint, "", ""));
    assert_eq!(
        control.output.status.code(),
        Some(0),
        "{}",
        control.context()
    );
    let first_input = only(&control.trace(), "inference")["input_tokens"]
        .as_u64()
        .unwrap();
    // Admits the first call (its input plus the cap) and refuses the second, which carries the
    // first call's input again plus the fact message.
    let ceiling = first_input + CAP + first_input / 2;

    let server = common::ScriptedServer::start(vec![capped_empty(), end_turn("two")]);
    let run = run(
        &home,
        &http_manifest(
            &server.endpoint,
            "",
            &format!("  max_session_tokens: {ceiling}\n"),
        ),
    );

    assert_eq!(server.requests().len(), 1, "{}", run.context());
    let trace = run.trace();
    assert_eq!(decisions(&trace), [pair("capped", "max_tokens")]);
    assert_eq!(
        events(&trace, "spend_ceiling_reached").len(),
        1,
        "{trace:#?}"
    );
    assert!(events(&trace, "task_failed").is_empty(), "{trace:#?}");
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "spend_ceiling_reached");
    assert_eq!(task_end["capped_turns"], 1);
}

// ── S8: keep-working sees only real turn ends ────────────────────────────────

/// Under `lifecycle.keep_working` a capped turn is not a turn end: the fact comes first, and the
/// keep-working message comes once, after the first real turn end.
#[test]
fn keep_working_fires_only_after_a_real_turn_end() {
    let home = home_with_driver();
    let server =
        common::ScriptedServer::start(vec![capped_empty(), end_turn("one"), end_turn("two")]);
    let lifecycle = format!("  keep_working:\n    message: \"{KEEP_WORKING}\"\n    limit: 1\n");
    let run = run(&home, &http_manifest(&server.endpoint, &lifecycle, ""));
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    assert_eq!(
        sole_text(messages(&requests[1]).last().unwrap()),
        no_call_fact()
    );
    assert!(!requests[1].to_string().contains(KEEP_WORKING));
    assert_eq!(
        sole_text(messages(&requests[2]).last().unwrap()),
        KEEP_WORKING
    );

    let trace = run.trace();
    let inferences = positions(&trace, "inference");
    let kept = positions(&trace, "keep_working");
    assert_eq!(kept.len(), 1, "{trace:#?}");
    assert!(
        inferences[1] < kept[0] && kept[0] < inferences[2],
        "the continuation follows the first real turn end: {trace:#?}"
    );
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(task_end["capped_turns"], 1);
    assert_eq!(
        task_end["keep_working"],
        json!({"used": 1, "limit": 1, "stopped_by": "limit"})
    );
    assert_eq!(run.result(), "two");
}

/// The trace written by S2's shape, read back by `mur trace show`: the capped-turn count is
/// printed under the turn count.
#[test]
fn trace_show_prints_the_capped_turn_count() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![capped_empty(), end_turn("done")]);
    let run = run(&home, &http_manifest(&server.endpoint, "", ""));
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let trace: &Path = &run.records_dir().join("trace.jsonl");
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "show", trace.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line == "capped:     1 turn stopped at the inference.max_tokens output cap"),
        "{stdout}"
    );
}
