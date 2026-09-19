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

use common::hook_wat::{
    compaction_hook_wasm, compaction_hook_wasm_as, conversation_reading_task_end_hook_wasm,
    create_hook_zip, mark_reporting_compaction_hook_wasm, MARK_REPORT_SEP, SEED_CONTEXT,
};

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
    tool_call_named("toolu_1", "bash")
}

/// A capsule manifest declaring the driver, `blocks` of extra top-level YAML, and the hooks.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    blocks: &str,
    hook_names: &[&str],
) -> PathBuf {
    create_manifest_named(project_dir, "murmur.yaml", endpoint, blocks, hook_names)
}

/// [`create_manifest`] under `file_name`, so one project directory — and so one session root —
/// can hold several capsules that share a name and a record.
fn create_manifest_named(
    project_dir: &Path,
    file_name: &str,
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
         version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
         endpoint: {endpoint}\n      api_key: test-key\n{hooks}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  \
         driver:\n    artifact: {DRIVER_NAME}\n",
    );
    let path = project_dir.join(file_name);
    fs::write(&path, manifest).unwrap();
    path
}

/// Grant the hook entry `hook_name` in `manifest` `capabilities.conversation.read`.
fn grant_conversation_read(manifest: &Path, hook_name: &str) {
    let entry = format!("  - name: {hook_name}\n    version: 0.1.0\n    runtime: hook\n");
    let text = fs::read_to_string(manifest).unwrap();
    assert!(text.contains(&entry), "{hook_name} is declared: {text}");
    let granted = text.replace(
        &entry,
        &format!("{entry}    capabilities:\n      conversation:\n        read: true\n"),
    );
    fs::write(manifest, granted).unwrap();
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

/// The mark a record line carries, or `None` for a line no hook output inserted.
fn inserted_by_of(message: &Value) -> Option<&str> {
    message.get("inserted_by").map(|mark| {
        mark.as_str()
            .unwrap_or_else(|| panic!("inserted_by is a string when present: {message}"))
    })
}

/// Run a capsule whose compaction hook fires on the first turn and returns `hook`'s summary, and
/// return the record it left alongside every request body the provider received.
fn compacted_run(summary: &str, hook: Vec<u8>) -> (Vec<Value>, Vec<Value>) {
    let f = fixture(
        vec![tool_call(), end_turn("done")],
        // Small enough that the first turn's occupancy is over the threshold, so compaction
        // fires on turn 1 and the summary joins the record.
        "context:\n  max_tokens: 100\n",
        &[("compactor", "on-compaction", "replace-context", hook)],
    );

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]);

    let messages = record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID);
    assert!(
        messages
            .iter()
            .any(|message| message["content"].to_string().contains(summary)),
        "the compaction hook fired and its summary is a line: {messages:#?}"
    );
    (messages, f.server.requests())
}

/// A person opening the record can tell the compaction summary from their own turn: the runtime
/// marks the line it committed from `replace-context`, and nothing else. The mark stays in the
/// record and never reaches the provider.
#[test]
fn a_compaction_summary_is_marked_in_the_record() {
    const SUMMARY: &str = "everything so far, in one line";
    let (messages, requests) = compacted_run(SUMMARY, compaction_hook_wasm(SUMMARY));

    for message in &messages {
        let is_summary = message["content"].to_string().contains(SUMMARY);
        let expected = is_summary.then_some("replace-context");
        assert_eq!(inserted_by_of(message), expected, "{messages:#?}");
    }
    for role in ["user", "assistant", "tool"] {
        assert!(
            messages.iter().any(|message| message["role"] == role
                && !message["content"].to_string().contains(SUMMARY)),
            "the record holds an unmarked {role} line to check: {messages:#?}"
        );
    }
    assert!(
        messages
            .iter()
            .any(|message| message["content"].to_string().contains(TASK_TEXT)
                && inserted_by_of(message).is_none()),
        "the person's task line is unmarked: {messages:#?}"
    );

    for request in &requests {
        let body = request.to_string();
        assert!(!body.contains("inserted_by"), "{body}");
        assert!(!body.contains("inserted-by"), "{body}");
    }
}

/// The mark follows the output a message came in through, not its role, and not what the hook
/// claims: a summary returned as `assistant` with a forged `seed-context` mark is recorded as
/// the `replace-context` it is.
#[test]
fn an_assistant_role_summary_is_still_marked() {
    const SUMMARY: &str = "the assistant's own recap";
    let (messages, _) = compacted_run(
        SUMMARY,
        compaction_hook_wasm_as("assistant", SUMMARY, Some(SEED_CONTEXT)),
    );

    let summaries: Vec<&Value> = messages
        .iter()
        .filter(|message| message["content"].to_string().contains(SUMMARY))
        .collect();
    assert!(!summaries.is_empty());
    for summary in summaries {
        assert_eq!(summary["role"], "assistant", "{summary}");
        assert_eq!(
            inserted_by_of(summary),
            Some("replace-context"),
            "{summary}"
        );
    }
}

/// One entry of a hook's mark report: the role, the `inserted-by` it read (`r`, `s` or `-`), and
/// the text of the message's first content block, or `None` for a message with no text block.
#[derive(Debug)]
struct Seen {
    role: String,
    mark: char,
    text: Option<String>,
}

/// Parse a report a `common::hook_wat` reporting hook produced.
fn mark_report(report: &str) -> Vec<Seen> {
    assert!(!report.starts_with('!'), "the hook's read failed: {report}");
    report
        .split(MARK_REPORT_SEP)
        .map(|entry| {
            let mut fields = entry.splitn(3, '=');
            let (Some(role), Some(mark), Some(content)) =
                (fields.next(), fields.next(), fields.next())
            else {
                panic!("malformed report entry {entry:?} in {report:?}");
            };
            let content: Value =
                serde_json::from_str(content).unwrap_or_else(|_| Value::from(content));
            Seen {
                role: role.to_string(),
                mark: mark.chars().next().expect("every entry carries a mark"),
                text: content[0]["text"].as_str().map(str::to_string),
            }
        })
        .collect()
}

/// Every entry of `seen` whose text is exactly `text`, asserting there is at least one.
fn seen_with_text<'a>(seen: &'a [Seen], text: &str) -> Vec<&'a Seen> {
    let matching: Vec<&Seen> = seen
        .iter()
        .filter(|entry| entry.text.as_deref() == Some(text))
        .collect();
    assert!(
        !matching.is_empty(),
        "the hook saw a message reading {text:?}: {seen:#?}"
    );
    matching
}

/// Every event of `event_type` in a session's trace.
fn trace_events(workdir: &Path, event_type: &str) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// What the reading hook saw through `murmur:conversation/read`, taken off the `task_reopened`
/// line its report became.
fn read_through_conversation(workdir: &Path) -> Vec<Seen> {
    let reopened = trace_events(workdir, "task_reopened");
    assert_eq!(reopened.len(), 1, "the reader reopens once: {reopened:?}");
    mark_report(reopened[0]["reason"].as_str().unwrap())
}

/// A hook reading the conversation through `murmur:conversation/read` after a real compaction
/// tells the summary from the person's turn by `inserted-by` alone. The summary role is the
/// hook's choice, so both are driven: under `user` the two messages share a role and differ only
/// by the mark.
#[test]
fn a_reader_tells_a_real_compaction_summary_from_a_persons_turn() {
    const SUMMARY: &str = "the conversation so far, summarised";
    for role in ["user", "assistant"] {
        let f = fixture(
            vec![tool_call(), end_turn("done"), end_turn("reopened")],
            "context:\n  max_tokens: 100\n",
            &[
                (
                    "compactor",
                    "on-compaction",
                    "replace-context",
                    compaction_hook_wasm_as(role, SUMMARY, None),
                ),
                (
                    "reader",
                    "on-task-end",
                    "reopen-task",
                    conversation_reading_task_end_hook_wasm(),
                ),
            ],
        );
        grant_conversation_read(&f.manifest, "reader");

        let workdir = workdir_of(run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success());
        let seen = read_through_conversation(&workdir);

        for person in seen_with_text(&seen, TASK_TEXT) {
            assert_eq!(
                (person.role.as_str(), person.mark),
                ("user", '-'),
                "{seen:#?}"
            );
        }
        for summary in seen_with_text(&seen, SUMMARY) {
            assert_eq!(
                (summary.role.as_str(), summary.mark),
                (role, 'r'),
                "{seen:#?}"
            );
        }
        for other in seen
            .iter()
            .filter(|entry| entry.text.as_deref() != Some(SUMMARY))
        {
            assert_eq!(other.mark, '-', "only the summary is marked: {seen:#?}");
        }
        drop(f.project);
    }
}

/// The mark a run commits is still there when a later run loads the record, whether
/// `lifecycle.conversation: threaded` reloads it or `--resume` does: the compaction hook of the
/// later run is handed the earlier summary marked and the earlier task unmarked, a reader sees the
/// same through `murmur:conversation/read`, and the earlier run's lines are never rewritten.
#[test]
fn the_mark_survives_a_threaded_reload_and_a_resume() {
    const SUMMARY: &str = "the first run, summarised";
    let f = fixture(
        vec![
            tool_call(),
            end_turn("done"),
            end_turn("reloaded"),
            end_turn("resumed"),
            end_turn("resumed and reopened"),
        ],
        "context:\n  max_tokens: 100\n",
        &[(
            "compactor",
            "on-compaction",
            "replace-context",
            compaction_hook_wasm(SUMMARY),
        )],
    );
    for (name, binding, commit_policy, wasm) in [
        (
            "reporter",
            "on-compaction",
            "replace-context",
            mark_reporting_compaction_hook_wasm(),
        ),
        (
            "reader",
            "on-task-end",
            "reopen-task",
            conversation_reading_task_end_hook_wasm(),
        ),
    ] {
        let artifact = create_hook_zip(f._artifacts.path(), name, binding, commit_policy, &wasm);
        common::publish_local(&f.home, &artifact).success();
    }
    let threaded = create_manifest_named(
        f.project.path(),
        "threaded.yaml",
        &f.server.endpoint,
        "context:\n  max_tokens: 100\nlifecycle:\n  conversation: threaded\n",
        &["reporter"],
    );
    let resumed = create_manifest_named(
        f.project.path(),
        "resumed.yaml",
        &f.server.endpoint,
        "context:\n  max_tokens: 100\n",
        &["reporter", "reader"],
    );
    grant_conversation_read(&resumed, "reader");

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]).success();
    let first = record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID);

    // What the later run's compaction hook was handed, off the summary it wrote in reply.
    let handed_in_run = |from_line: usize| -> Vec<Seen> {
        let messages = record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID);
        let report = messages[from_line..]
            .iter()
            .find(|message| inserted_by_of(message) == Some("replace-context"))
            .unwrap_or_else(|| panic!("the reporter's summary is a line: {messages:#?}"));
        mark_report(report["content"][0]["text"].as_str().unwrap())
    };
    let assert_first_run_marks = |seen: &[Seen], view: &str| {
        for summary in seen_with_text(seen, SUMMARY) {
            assert_eq!(summary.mark, 'r', "{view}: {seen:#?}");
        }
        for person in seen_with_text(seen, TASK_TEXT) {
            assert_eq!(person.mark, '-', "{view}: {seen:#?}");
        }
    };

    // The three manifests share one project directory, and so one `murmur.lock`; each launch
    // resolves its own artifacts afresh when none is there.
    let unlock = || fs::remove_file(f.project.path().join("murmur.lock")).unwrap();

    unlock();
    run(&f.home, &threaded, &["--context", CONTEXT_ID]).success();
    let reloaded = record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID);
    assert_eq!(&reloaded[..first.len()], &first[..], "no line is rewritten");
    assert_first_run_marks(
        &handed_in_run(first.len()),
        "threaded reload, compaction event",
    );

    // `@1` is the threaded run, the latest session under this project's session root.
    unlock();
    let workdir = workdir_of(run(&f.home, &resumed, &["--resume", "@1"]).success());
    let after = record_lines(&f.home, CAPSULE_NAME, CONTEXT_ID);
    assert_eq!(
        &after[..reloaded.len()],
        &reloaded[..],
        "no line is rewritten"
    );
    assert_first_run_marks(&handed_in_run(reloaded.len()), "resume, compaction event");
    assert_first_run_marks(
        &read_through_conversation(&workdir),
        "resume, conversation/read",
    );

    for request in f.server.requests() {
        let body = request.to_string();
        assert!(!body.contains("inserted_by"), "{body}");
        assert!(!body.contains("inserted-by"), "{body}");
    }
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

// ── fence labels ─────────────────────────────────────────────────────────────

/// One Anthropic response asking for `name`, so a turn can exercise any dispatch shape. Scripts
/// the same `echo hello` input whatever the tool, which a skill and an undeclared tool both ignore.
fn tool_call_named(call_id: &str, name: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": {"command": "echo hello"}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// The `fence` key on a record line, or `None` when the line carries none.
fn fence_of(message: &Value) -> Option<&str> {
    message.get("fence").map(|value| {
        value
            .as_str()
            .unwrap_or_else(|| panic!("the fence key is a string: {message}"))
    })
}

/// The record labels exactly the lines whose content is fenced.
///
/// One run, four turns: a declared shell tool (fenced), a declared skill (never fenced), an
/// undeclared tool whose dispatch fails before any tool ran (never fenced), and a plain reply.
/// A consumer reading `conversation.jsonl` keys off `fence` rather than matching a marker
/// against the content — and the content still carries the markers either way, because the
/// record is what the next run replays into the model's context.
#[test]
fn the_record_labels_a_fenced_tool_message() {
    // The shell grant below makes this capsule need host support the rest of this file does not.
    if common::skip_without_host_support("the_record_labels_a_fenced_tool_message") {
        return;
    }
    const SKILL_NAME: &str = "house-style";
    const SKILL_TEXT: &str = "# House style\n\nPrefer short sentences.";

    let f = fixture(
        vec![
            tool_call(),
            tool_call_named("toolu_skill", SKILL_NAME),
            tool_call_named("toolu_missing", "no-such-tool"),
            end_turn("done"),
        ],
        "",
        &[],
    );

    let skill = common::create_skill_artifact(f._artifacts.path(), SKILL_NAME, "0.1.0", SKILL_TEXT);
    common::publish_local(&f.home, &skill).success();

    // `create_manifest` writes neither a shell grant nor a skill entry, and both are needed for
    // this run's first two turns to dispatch rather than fail.
    let manifest = fs::read_to_string(&f.manifest)
        .unwrap()
        .replace(
            "capabilities:\n  network:\n",
            "capabilities:\n  shell:\n    allow:\n      - bash\n  network:\n",
        )
        .replace(
            "      api_key: test-key\n",
            &format!("      api_key: test-key\n  - name: {SKILL_NAME}\n    version: 0.1.0\n    runtime: skill\n"),
        );
    fs::write(&f.manifest, manifest).unwrap();

    run(&f.home, &f.manifest, &["--context", CONTEXT_ID]);

    let messages = record_messages(&f.home, CAPSULE_NAME, CONTEXT_ID);
    let labelled: Vec<(&str, Option<&str>)> = messages
        .iter()
        .map(|message| (message["role"].as_str().unwrap(), fence_of(message)))
        .collect();
    assert_eq!(
        labelled,
        vec![
            // The task, launched locally, so `user` origin and trusted.
            ("user", None),
            ("assistant", None),
            ("tool", Some("tool:bash")),
            ("assistant", None),
            ("tool", None),
            ("assistant", None),
            ("tool", None),
            ("assistant", None),
        ],
        "{messages:#?}"
    );

    let fenced_text = messages[2]["content"][0]["text"].as_str().unwrap();
    assert!(
        fenced_text.starts_with("<untrusted-content source=tool:bash>")
            && fenced_text.ends_with("</untrusted-content>"),
        "a labelled line still carries the markers verbatim: {fenced_text}"
    );

    let skill_text = messages[4]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        skill_text, SKILL_TEXT,
        "a skill result is unlabelled and unfenced"
    );

    let failure_text = messages[6]["content"][0]["text"].as_str().unwrap();
    assert!(
        !failure_text.contains("untrusted-content"),
        "a dispatch failure is unlabelled and unfenced: {failure_text}"
    );

    assert!(
        messages
            .iter()
            .all(|message| message.get("inserted_by").is_none()),
        "no hook output inserted any of these lines: {messages:#?}"
    );

    // What the record labels is never what the driver receives.
    for request in f.server.requests() {
        for message in request_messages(&request) {
            assert!(
                message.get("fence").is_none(),
                "the fence key must not reach the driver: {message}"
            );
        }
    }
    drop(f.project);
}
