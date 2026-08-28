//! The durable conversation record: what `~/.murmur/conversations/<record>/<context-id>/` holds
//! after a run, and what turns it off.
//!
//! Every case drives the real `mur` binary against a real Wasmtime driver, because the property
//! under test is a host path *outside* every session workdir: two launches that share nothing but
//! a home directory and a context id have to land in one file, and only a real launch can show
//! that. The hook components are hand-authored WAT compiled in-test, so nothing here depends on a
//! `default-artifacts` checkout and no case is `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, Command};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::hook_wat::{compaction_hook_wasm, create_hook_zip};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "record-capsule";
const TASK_TEXT: &str = "Report what you remember.";
const CONTEXT_ID: &str = "ctx_fixed";

/// The record one context wrote.
fn record_path(home: &TempDir, record: &str, context_id: &str) -> PathBuf {
    home.path()
        .join(".murmur/conversations")
        .join(record)
        .join(context_id)
        .join("conversation.jsonl")
}

/// Every line of a record, as raw text, so a second run's assertion can compare bytes.
fn record_lines(home: &TempDir, record: &str, context_id: &str) -> Vec<String> {
    let path = record_path(home, record, context_id);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every record line parsed, with the whole line quoted on a failure: a line that does not parse
/// is exactly the defect these tests exist to catch.
fn record_messages(home: &TempDir, record: &str, context_id: &str) -> Vec<Value> {
    record_lines(home, record, context_id)
        .iter()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("line is not JSON ({err}): {line}"))
        })
        .collect()
}

/// Whether `id` is one the runtime minted: `msg_` and 32 lowercase hex digits.
fn is_message_id(id: &str) -> bool {
    id.len() == 36
        && id.starts_with("msg_")
        && id[4..]
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn assert_dir_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .unwrap_or_else(|err| panic!("{} must exist: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode,
        expected,
        "{} must be {expected:o}, got {mode:04o}",
        path.display()
    );
}

/// One Anthropic response that ends the turn.
fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// One Anthropic response that asks for a tool, so the loop takes the `tool_call` arm.
fn tool_call() -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": "toolu_1",
            "name": "bash",
            "input": {"command": "echo hello"}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// A capsule manifest declaring the driver, `blocks` of extra top-level YAML, and the hooks.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    blocks: &str,
    hook_names: &[&str],
) -> PathBuf {
    let hooks: String = hook_names
        .iter()
        .map(|name| format!("  - name: {name}\n    version: 0.1.0\n    runtime: hook\n"))
        .collect();
    let manifest = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\n{blocks}artifacts:\n  - name: {DRIVER_NAME}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n{hooks}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  \
         model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n",
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// Publish the driver, and each hook, into `home`'s artifact store.
fn publish_artifacts(home: &TempDir, artifact_dir: &Path, hooks: &[(&str, &str, &str, Vec<u8>)]) {
    let driver = common::create_driver_artifact(
        artifact_dir,
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver).success();
    for (name, binding, commit_policy, wasm) in hooks {
        let artifact = create_hook_zip(artifact_dir, name, binding, commit_policy, wasm);
        common::publish_local(home, &artifact).success();
    }
}

/// `mur run` with the task inline, plus whatever flags the case needs.
fn run(home: &TempDir, manifest: &Path, extra: &[&str]) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    cmd.args([
        "run",
        "--manifest",
        manifest.to_str().unwrap(),
        "--task",
        TASK_TEXT,
        "--verbose",
    ]);
    cmd.args(extra);
    cmd.assert()
}

/// A launched run's session workdir, read off the line `mur run --verbose` prints.
fn workdir_of(assert: Assert) -> PathBuf {
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    common::parse_workdir_from_stdout(&stdout)
}

/// One project, its temp home and its scripted provider, wired together.
struct Fixture {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    server: common::ScriptedServer,
    manifest: PathBuf,
}

fn fixture(responses: Vec<String>, blocks: &str, hooks: &[(&str, &str, &str, Vec<u8>)]) -> Fixture {
    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_artifacts(&home, artifacts.path(), hooks);
    let manifest = create_manifest(
        project.path(),
        &server.endpoint,
        blocks,
        &hooks_names(hooks),
    );
    Fixture {
        home,
        project,
        _artifacts: artifacts,
        server,
        manifest,
    }
}

fn hooks_names<'a>(hooks: &'a [(&'a str, &'a str, &'a str, Vec<u8>)]) -> Vec<&'a str> {
    hooks.iter().map(|(name, _, _, _)| *name).collect()
}

/// The messages one request carried.
fn request_messages(request: &Value) -> &Vec<Value> {
    request["messages"]
        .as_array()
        .expect("a driver request carries a messages array")
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// The whole point of the record: two launches, two session workdirs, one file. Neither run
/// passes `--workdir`, so they share nothing but `HOME` and the context id, and the second run's
/// lines can only follow the first run's if the record outlived the session that wrote it.
#[test]
fn the_record_outlives_the_session_and_a_second_run_appends() {
    let f = fixture(vec![end_turn("first"), end_turn("second")], "", &[]);

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();
    let after_first = record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID);
    assert_eq!(
        after_first.len(),
        2,
        "the task's user message and the assistant reply: {after_first:?}"
    );

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();
    let after_second = record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID);
    assert_eq!(
        &after_second[..2],
        &after_first[..],
        "the first run's lines must survive byte for byte"
    );
    assert_eq!(after_second.len(), 4, "and the second run's follow them");

    for message in record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID) {
        let id = message["id"].as_str().expect("every line carries an id");
        assert!(is_message_id(id), "malformed id: {id}");
    }

    // The record is one capsule's whole conversation, so every directory on the path is
    // owner-only — a readable root leaks record names.
    assert_dir_mode(&f.home.path().join(".murmur/conversations"), 0o700);
    assert_dir_mode(
        &f.home
            .path()
            .join(".murmur/conversations")
            .join(CAPSULE_NAME),
        0o700,
    );
    assert_dir_mode(
        &f.home
            .path()
            .join(".murmur/conversations")
            .join(CAPSULE_NAME)
            .join(CONTEXT_ID),
        0o700,
    );
    drop(f.project);
}

/// A task that fails has still put a message in front of the model, so the record holds it. The
/// record is written as the context is built, not at a terminal arm the failure never reaches.
#[test]
fn a_failed_task_records_what_it_sent() {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_artifacts(&home, artifacts.path(), &[]);
    // A port nothing is listening on: the driver's request fails, which is a failed inference and
    // a failed task, and no scripted response can be mistaken for one.
    let manifest = create_manifest(project.path(), "http://127.0.0.1:1", "", &[]);

    let workdir = workdir_of(run(&home, &manifest, &["--context", CONTEXT_ID]));

    let result = fs::read_to_string(workdir.join("out/result.txt")).unwrap_or_default();
    assert!(
        result.starts_with("error:"),
        "the task must have failed: {result}"
    );

    let messages = record_messages(&home, CAPSULE_NAME, CONTEXT_ID);
    assert_eq!(messages.len(), 1, "the task's user message: {messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert!(is_message_id(messages[0]["id"].as_str().unwrap()));
}

/// A task that spends `inference.max_turns` never reaches a terminal arm either, and its one turn
/// is recorded in full: the assistant message that asked for a tool, then the tool result.
#[test]
fn a_task_that_spends_its_turns_records_every_turn_it_took() {
    let f = fixture(vec![tool_call()], "inference_max_turns_placeholder\n", &[]);
    // `max_turns` belongs inside the `inference:` block, which `create_manifest` writes last.
    let manifest = fs::read_to_string(&f.manifest)
        .unwrap()
        .replace("inference_max_turns_placeholder\n", "")
        .replace("  transport: http\n", "  transport: http\n  max_turns: 1\n");
    fs::write(&f.manifest, manifest).unwrap();

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]);

    let messages = record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID);
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "tool"], "{messages:?}");
    for message in &messages {
        assert!(is_message_id(message["id"].as_str().unwrap()));
    }
    drop(f.project);
}

/// `lifecycle.conversation` governs what a task *loads*, never what the record holds: both modes
/// write both runs, and only `threaded` starts the second run from the first.
#[test]
fn stateless_writes_the_record_without_reloading_it() {
    let f = fixture(vec![end_turn("first"), end_turn("second")], "", &[]);

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();
    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();

    let requests = f.server.requests();
    assert_eq!(request_messages(&requests[0]).len(), 1);
    assert_eq!(
        request_messages(&requests[1]).len(),
        1,
        "a stateless task starts from its own message alone"
    );
    assert_eq!(record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID).len(), 4);
    drop(f.project);
}

#[test]
fn threaded_reloads_the_record_into_the_next_run() {
    let f = fixture(
        vec![end_turn("first"), end_turn("second")],
        "lifecycle:\n  conversation: threaded\n",
        &[],
    );

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();
    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();

    let requests = f.server.requests();
    assert_eq!(request_messages(&requests[0]).len(), 1);
    let second = request_messages(&requests[1]);
    assert_eq!(
        second.len(),
        3,
        "the first run's two messages, then this task's: {second:?}"
    );
    assert_eq!(second[0]["role"], "user");
    assert_eq!(second[1]["role"], "assistant");
    assert_eq!(second[2]["role"], "user");
    assert_eq!(
        record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID).len(),
        4,
        "a reloaded message is never written a second time"
    );
    drop(f.project);
}

/// `context.record: off` is the whole mechanism off: no root, no record directory, no file.
#[test]
fn record_off_creates_nothing_anywhere() {
    let f = fixture(vec![end_turn("done")], "context:\n  record: off\n", &[]);

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();

    assert!(
        !f.home.path().join(".murmur/conversations").exists(),
        "record: off must create nothing at all"
    );
    drop(f.project);
}

/// `context.record_store` names the directory; the capsule name is only the default.
#[test]
fn an_explicit_record_store_is_the_only_directory_created() {
    let f = fixture(
        vec![end_turn("done")],
        "context:\n  record_store: shey\n",
        &[],
    );

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();

    assert_eq!(record_lines(&f.home, "shey", CONTEXT_ID).len(), 2);
    assert!(
        !f.home
            .path()
            .join(".murmur/conversations")
            .join(CAPSULE_NAME)
            .exists(),
        "the capsule-named default must not be created beside a declared record"
    );
    drop(f.project);
}

/// Every message in the record carries its own well-formed id, including the ones a compaction
/// hook produced and the ones it replaced — both are in the conversation, so both are lines.
#[test]
fn every_recorded_message_carries_a_unique_well_formed_id() {
    let f = fixture(
        vec![tool_call(), end_turn("done")],
        // Small enough that the first turn's occupancy is over the threshold, so compaction
        // fires on turn 1 and the summary joins the record.
        "context:\n  max_tokens: 100\n",
        &[(
            "compactor",
            "on-compaction",
            "replace-context",
            compaction_hook_wasm("everything so far, in one line"),
        )],
    );

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]);

    let messages = record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID);
    let mut ids: Vec<&str> = messages
        .iter()
        .map(|message| message["id"].as_str().expect("every line carries an id"))
        .collect();
    for id in &ids {
        assert!(is_message_id(id), "malformed id: {id}");
    }
    ids.sort_unstable();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), unique, "no id may appear twice in one record");

    let texts: Vec<String> = messages
        .iter()
        .map(|message| message["content"].to_string())
        .collect();
    assert!(
        texts.iter().any(|text| text.contains(TASK_TEXT)),
        "the message the summary replaced is still a line: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("everything so far, in one line")),
        "the summary that replaced it is a line too: {texts:?}"
    );
    drop(f.project);
}

/// Each agent-loop `inference` line names the messages its request embedded, in order.
#[test]
fn the_inference_trace_event_names_the_messages_it_sent() {
    let f = fixture(vec![tool_call(), end_turn("done")], "", &[]);

    let workdir = workdir_of(run(&f.home, &f.manifest, &["--context", CONTEXT_ID]));

    let trace = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
    let inferences: Vec<Value> = trace
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event_type"] == "inference")
        .collect();
    assert_eq!(inferences.len(), 2, "two turns: {inferences:?}");

    let requests = f.server.requests();
    for (event, request) in inferences.iter().zip(&requests) {
        assert!(event.get("origin").is_none(), "an agent-loop turn: {event}");
        let ids = event["message_ids"]
            .as_array()
            .unwrap_or_else(|| panic!("every agent-loop inference names its messages: {event}"));
        assert_eq!(
            ids.len(),
            request_messages(request).len(),
            "one id per message the request embedded"
        );
        for id in ids {
            assert!(is_message_id(id.as_str().unwrap()), "malformed id: {id}");
        }
    }
    drop(f.project);
}

/// Both halves of a record path are one directory segment, and a value that is not refuses the
/// launch before anything is created.
#[test]
fn a_context_id_that_is_not_one_segment_refuses_the_launch() {
    let f = fixture(vec![end_turn("done")], "", &[]);

    run(&f.home, &f.manifest, &["--context", "../escape"])
        .failure()
        .stderr(predicate::str::contains("E-CAP-011"))
        .stderr(predicate::str::contains("'../escape'"))
        .stderr(predicate::str::contains("single path segment"));

    assert!(!f.home.path().join(".murmur/conversations").exists());
    drop(f.project);
}

#[test]
fn a_record_store_that_is_not_one_segment_refuses_the_launch() {
    let f = fixture(
        vec![end_turn("done")],
        "context:\n  record_store: a/b\n",
        &[],
    );

    run(&f.home, &f.manifest, &[])
        .failure()
        .stderr(predicate::str::contains("E-CAP-011"))
        .stderr(predicate::str::contains("'a/b'"))
        .stderr(predicate::str::contains("single path segment"));

    assert!(!f.home.path().join(".murmur/conversations").exists());
    drop(f.project);
}
