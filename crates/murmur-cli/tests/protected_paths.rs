//! End-to-end coverage for `capabilities.filesystem.read_only`: what the manifest refuses before
//! dispatch, what it deliberately does not refuse, what the agent is told, and what `trace.jsonl`
//! records about a call that never ran.
//!
//! Everything here runs on a bare host — real `bash`, real files, a real `launch_session` against
//! a scripted inference driver. No container, no network, no `default-artifacts` checkout, and
//! nothing `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::launch_session;
use predicates::prelude::*;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The protected file every scenario aims at, and its original bytes. That the bytes are
/// unchanged is the measurement a refusal has to survive.
const PROTECTED_FILE: &str = "tests/test_foo.py";
const PROTECTED_CONTENTS: &str = "def test_foo():\n    assert compute() == 42\n";

/// The file the fixture tool creates when it actually runs. Its absence is how a scenario shows
/// the tool was never dispatched.
const MARKER: &str = "marker.txt";

/// A native tool that writes [`MARKER`] and reports success. It ignores its input entirely: what
/// is under test is whether the runtime dispatches it at all.
fn writer_tool_script() -> String {
    format!(
        "#!/bin/sh\n: > {MARKER}\necho '{}'\n",
        r#"{"status":"passed","summary":"ok","data":"wrote","data_path":null,"truncated":false,"metadata":[]}"#
    )
}

// ── Fixtures ─────────────────────────────────────────────────────────────────

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

/// What one launched session left behind.
struct Session {
    requests: Vec<Value>,
    trace: Vec<Value>,
    trace_raw: String,
    trace_path: PathBuf,
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

    /// The one `protected_path_denied` line, with the whole trace in the failure message — a
    /// refusal that did not get recorded is the defect these tests exist to catch.
    fn refusal(&self) -> &Value {
        let refusals = self.events("protected_path_denied");
        assert_eq!(
            refusals.len(),
            1,
            "expected exactly one protected_path_denied line; trace was:\n{}",
            self.trace_raw
        );
        refusals[0]
    }

    fn protected_file_contents(&self) -> String {
        fs::read_to_string(self.workdir.join(PROTECTED_FILE)).unwrap()
    }

    fn marker_exists(&self) -> bool {
        self.workdir.join(MARKER).exists()
    }

    fn tool_result_text(&self, tool_use_id: &str) -> String {
        let block = common::find_tool_result(&self.requests, tool_use_id)
            .unwrap_or_else(|| panic!("no tool_result for {tool_use_id}"));
        common::extract_result_text(&block)
    }
}

/// One capsule to stage: what it declares read-only, which binaries it allows, and which native
/// tools it carries.
///
/// Each tool is a name and the `input_schema` its artifact manifest carries — `None` for a tool
/// that publishes no schema, which is what leaves it on the key-name rules.
struct Capsule<'a> {
    read_only: &'a [&'a str],
    shell_allow: &'a [&'a str],
    tools: &'a [(&'a str, Option<&'a str>)],
}

fn capsule<'a>(read_only: &'a [&'a str], shell_allow: &'a [&'a str]) -> Capsule<'a> {
    Capsule {
        read_only,
        shell_allow,
        tools: &[],
    }
}

fn create_manifest(project_dir: &Path, endpoint: &str, capsule: &Capsule<'_>) -> PathBuf {
    let tool_yaml: String = capsule
        .tools
        .iter()
        .map(|(name, _)| format!("  - name: {name}\n    version: 0.1.0\n    runtime: tool\n"))
        .collect();
    let shell_yaml: String = capsule
        .shell_allow
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect();
    let read_only_yaml: String = capsule
        .read_only
        .iter()
        .map(|entry| format!("      - {entry}\n"))
        .collect();

    let manifest = format!(
        "name: protected-capsule\n\
         version: 0.1.0\n\
         artifacts:\n\
         \x20 - name: {DRIVER_NAME}\n\
         \x20   version: {DRIVER_VERSION}\n\
         \x20   runtime: driver\n\
         {tool_yaml}\
         capabilities:\n\
         \x20 network:\n\
         \x20   allow:\n\
         \x20     - {endpoint}\n\
         \x20 shell:\n\
         \x20   allow:\n\
         {shell_yaml}\
         \x20 filesystem:\n\
         \x20   read_only:\n\
         {read_only_yaml}\
         inference:\n\
         \x20 transport: http\n\
         \x20 endpoint: {endpoint}\n\
         \x20 model: test-model\n\
         \x20 api_key: test-key\n\
         \x20 driver:\n\
         \x20   artifact: {DRIVER_NAME}\n"
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// Publish the driver and every native tool, stage the capsule, seed its workdir, run it, and
/// collect what it left.
///
/// `seed` runs after staging and before launch, so a scenario can put the protected file — or a
/// symlink into it — in place on the same workdir the session will use.
fn run_session(responses: Vec<String>, capsule: &Capsule<'_>, seed: impl FnOnce(&Path)) -> Session {
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

    for (name, schema) in capsule.tools {
        let artifact = common::create_native_artifact(
            artifact_dir.path(),
            name,
            "0.1.0",
            &writer_tool_script(),
            Some("Fixture writer tool"),
            *schema,
        );
        common::publish_local(&home, &artifact).success();
    }

    let manifest_path = create_manifest(project.path(), &server.endpoint, capsule);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();

    fs::create_dir_all(workdir.join("tests")).unwrap();
    fs::write(workdir.join(PROTECTED_FILE), PROTECTED_CONTENTS).unwrap();
    fs::create_dir_all(workdir.join("build")).unwrap();
    fs::write(workdir.join("task.md"), "Do the thing.").unwrap();
    seed(&workdir);

    launch_session(staged, |_| {}).expect("the session must launch whatever the manifest decides");

    let trace_path = workdir.join("trace.jsonl");
    let trace_raw = fs::read_to_string(&trace_path).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        requests: server.requests(),
        trace,
        trace_raw,
        trace_path,
        workdir,
        _project: project,
    }
}

/// One shell call the manifest is asked about, then an `end_turn`.
fn one_shell_call(binary: &str, command: &str) -> Vec<String> {
    vec![
        tool_call("msg_1", "toolu_shell", binary, json!({"command": command})),
        end_turn("msg_2", "Understood."),
    ]
}

/// One tool call the manifest is asked about, then an `end_turn`.
fn one_tool_call(name: &str, input: Value) -> Vec<String> {
    vec![
        tool_call("msg_1", "toolu_tool", name, input),
        end_turn("msg_2", "Understood."),
    ]
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// A `bash` redirection into a declared subtree is refused before anything is spawned: the file
/// keeps its bytes, the agent is told the path and the rule, and the trace carries the refusal
/// and no `shell` record.
#[test]
fn a_shell_redirection_into_a_protected_subtree_is_refused() {
    if common::skip_without_host_support("a_shell_redirection_into_a_protected_subtree_is_refused")
    {
        return;
    }
    let session = run_session(
        one_shell_call("bash", &format!("echo broken > {PROTECTED_FILE}")),
        &capsule(&["tests"], &["bash"]),
        |_| {},
    );

    assert_eq!(
        session.protected_file_contents(),
        PROTECTED_CONTENTS,
        "no subprocess ran, so the protected file is byte-identical"
    );

    let text = session.tool_result_text("toolu_shell");
    assert!(text.contains(PROTECTED_FILE), "names the path: {text}");
    assert!(text.contains("'tests'"), "names the rule: {text}");
    assert!(text.contains("Nothing ran"), "{text}");
    assert!(
        text.contains("still readable"),
        "the agent is told what is still true: {text}"
    );

    let refusal = session.refusal();
    assert_eq!(refusal["call"], "shell");
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], "tests");
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("redirection") && signal.contains('>'),
        "the signal names the redirection: {signal}"
    );

    assert!(
        session.events("shell").is_empty(),
        "nothing ran, so nothing is recorded as having run:\n{}",
        session.trace_raw
    );
}

/// "Readable but not writable" means the read runs. A slice that refuses this has broken the
/// feature on the exact case it exists for.
#[test]
fn reading_a_protected_path_is_not_refused() {
    if common::skip_without_host_support("reading_a_protected_path_is_not_refused") {
        return;
    }
    let session = run_session(
        one_shell_call("bash", &format!("cat {PROTECTED_FILE}")),
        // `cat` is allowlisted so a Landlock-enforcing host grants exec on it: what is under test
        // is the read-only rule, not the shell allowlist.
        &capsule(&["tests"], &["bash", "cat"]),
        |_| {},
    );

    assert!(
        session.events("protected_path_denied").is_empty(),
        "a read is not a write:\n{}",
        session.trace_raw
    );
    assert_eq!(
        session.events("shell").len(),
        1,
        "the command ran:\n{}",
        session.trace_raw
    );
    let text = session.tool_result_text("toolu_shell");
    assert!(
        text.contains("assert compute() == 42"),
        "the contents come back: {text}"
    );
}

/// A rule names a subtree of the workdir root, not a path fragment: `tests` does not cover
/// `tests2`.
#[test]
fn a_sibling_directory_sharing_a_prefix_is_not_protected() {
    if common::skip_without_host_support("a_sibling_directory_sharing_a_prefix_is_not_protected") {
        return;
    }
    let session = run_session(
        one_shell_call("bash", "echo x > tests2/note.txt"),
        &capsule(&["tests"], &["bash"]),
        |workdir| fs::create_dir_all(workdir.join("tests2")).unwrap(),
    );

    assert!(
        session.events("protected_path_denied").is_empty(),
        "'tests' must not cover 'tests2':\n{}",
        session.trace_raw
    );
    assert_eq!(
        fs::read_to_string(session.workdir.join("tests2/note.txt")).unwrap(),
        "x\n"
    );
}

/// A symlink into a protected subtree resolves to the rule that covers its target, and the record
/// reports the resolved path rather than the form the model typed.
#[test]
#[cfg(unix)]
fn a_symlink_into_a_protected_subtree_does_not_evade_the_rule() {
    if common::skip_without_host_support(
        "a_symlink_into_a_protected_subtree_does_not_evade_the_rule",
    ) {
        return;
    }
    let session = run_session(
        one_shell_call("bash", "echo x > link/test_foo.py"),
        &capsule(&["tests"], &["bash"]),
        |workdir| std::os::unix::fs::symlink(workdir.join("tests"), workdir.join("link")).unwrap(),
    );

    assert_eq!(session.protected_file_contents(), PROTECTED_CONTENTS);
    let refusal = session.refusal();
    assert_eq!(refusal["rule"], "tests");
    assert_eq!(
        refusal["path"], PROTECTED_FILE,
        "the resolved path is recorded, not the spelling the model typed"
    );
}

/// A tool input pairing a protected path with content is refused, and the tool is never invoked.
#[test]
fn a_tool_call_pairing_a_protected_path_with_content_is_refused() {
    if common::skip_without_host_support(
        "a_tool_call_pairing_a_protected_path_with_content_is_refused",
    ) {
        return;
    }
    let session = run_session(
        one_tool_call("writer", json!({"path": PROTECTED_FILE, "content": "pass"})),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("writer", None)],
        },
        |_| {},
    );

    assert!(!session.marker_exists(), "the tool must never be invoked");
    assert!(
        !session
            .events("tool_call")
            .iter()
            .any(|e| e["tool_name"] == "writer"),
        "a refused tool call is not recorded as a tool call:\n{}",
        session.trace_raw
    );

    let text = session.tool_result_text("toolu_tool");
    assert!(text.contains(PROTECTED_FILE), "{text}");
    assert!(text.contains("'tests'"), "{text}");

    let refusal = session.refusal();
    assert_eq!(refusal["call"], "tool");
    assert_eq!(refusal["target"], "writer");
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], "tests");
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("path") && signal.contains("content"),
        "the signal names the pairing: {signal}"
    );
}

/// A path with no content key beside it is a read, and dispatches normally.
#[test]
fn a_tool_call_naming_a_protected_path_with_no_content_is_dispatched() {
    if common::skip_without_host_support(
        "a_tool_call_naming_a_protected_path_with_no_content_is_dispatched",
    ) {
        return;
    }
    let session = run_session(
        one_tool_call("writer", json!({"path": PROTECTED_FILE})),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("writer", None)],
        },
        |_| {},
    );

    assert!(session.marker_exists(), "the tool ran");
    assert!(
        session.events("protected_path_denied").is_empty(),
        "a path alone is a read:\n{}",
        session.trace_raw
    );
    assert!(
        session
            .events("tool_call")
            .iter()
            .any(|e| e["tool_name"] == "writer"),
        "the dispatch is recorded:\n{}",
        session.trace_raw
    );
}

/// A destination-shaped key is a write on its own, with no content key anywhere in the input.
#[test]
fn a_destination_key_is_a_write_on_its_own() {
    if common::skip_without_host_support("a_destination_key_is_a_write_on_its_own") {
        return;
    }
    let session = run_session(
        one_tool_call(
            "writer",
            json!({"source": "build/out.py", "destination": PROTECTED_FILE}),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("writer", None)],
        },
        |_| {},
    );

    assert!(!session.marker_exists(), "the tool must never be invoked");
    let refusal = session.refusal();
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], "tests");
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("destination"),
        "the signal names the destination key: {signal}"
    );
}

// ── What a tool's own input schema declares ──────────────────────────────────

/// A tool that declares an object opaque has the pair inside it read as data: the note is
/// recorded, not refused.
const NOTER_SCHEMA: &str =
    r#"{"type":"object","properties":{"note":{"type":"object","format":"murmur-opaque"}}}"#;

/// A batch editor that says which of its inputs is the destination, under an array step.
const BATCHER_SCHEMA: &str = r#"{"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string","format":"murmur-destination"}}}}}}"#;

/// A destination under a name no key table carries.
const RENDERER_SCHEMA: &str =
    r#"{"type":"object","properties":{"sink":{"type":"string","format":"murmur-destination"}}}"#;

/// An opaque payload beside a declared destination.
const SNEAKY_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","format":"murmur-destination"},"body":{"type":"object","format":"murmur-opaque"}}}"#;

/// `murmur-opaque` on a string, which is ignored.
const MISLABELED_SCHEMA: &str =
    r#"{"type":"object","properties":{"path":{"type":"string","format":"murmur-opaque"}}}"#;

/// A `{file, text}` pair inside a subtree the tool declared opaque is stored data, and the call
/// is dispatched.
#[test]
fn an_opaque_payload_carrying_a_path_and_text_is_recorded_not_refused() {
    if common::skip_without_host_support(
        "an_opaque_payload_carrying_a_path_and_text_is_recorded_not_refused",
    ) {
        return;
    }
    let session = run_session(
        one_tool_call(
            "noter",
            json!({"note": {"file": PROTECTED_FILE, "text": "the test is protected"}}),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("noter", Some(NOTER_SCHEMA))],
        },
        |_| {},
    );

    assert!(session.marker_exists(), "the tool ran");
    assert!(
        session.events("protected_path_denied").is_empty(),
        "a declared payload is data, not filesystem intent:\n{}",
        session.trace_raw
    );
    assert!(
        session
            .events("tool_call")
            .iter()
            .any(|e| e["tool_name"] == "noter"),
        "the dispatch is recorded:\n{}",
        session.trace_raw
    );
}

/// A tool that declares nothing is judged by key name: a nested path/content pair is refused
/// before dispatch.
#[test]
fn a_nested_path_and_content_pair_is_still_refused_without_a_schema() {
    if common::skip_without_host_support(
        "a_nested_path_and_content_pair_is_still_refused_without_a_schema",
    ) {
        return;
    }
    let session = run_session(
        one_tool_call(
            "batcher",
            json!({"edits": [{"path": PROTECTED_FILE, "content": "pass"}]}),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("batcher", None)],
        },
        |_| {},
    );

    assert!(!session.marker_exists(), "the tool must never be invoked");
    assert!(
        !session
            .events("tool_call")
            .iter()
            .any(|e| e["tool_name"] == "batcher"),
        "a refused tool call is not recorded as a tool call:\n{}",
        session.trace_raw
    );
    assert_eq!(session.protected_file_contents(), PROTECTED_CONTENTS);

    let refusal = session.refusal();
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], "tests");
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("path") && signal.contains("content"),
        "the signal names the pairing: {signal}"
    );
}

/// A declared destination is checked under an array step, and both the trace and `mur trace show`
/// name the location that triggered the refusal.
#[test]
fn a_declared_destination_under_an_array_names_its_location() {
    if common::skip_without_host_support("a_declared_destination_under_an_array_names_its_location")
    {
        return;
    }
    let session = run_session(
        one_tool_call(
            "batcher-declared",
            json!({"edits": [{"path": PROTECTED_FILE, "content": "pass"}]}),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("batcher-declared", Some(BATCHER_SCHEMA))],
        },
        |_| {},
    );

    assert!(!session.marker_exists(), "the tool must never be invoked");
    let refusal = session.refusal();
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], "tests");
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("edits[].path"),
        "the signal names the declared location: {signal}"
    );
    assert!(
        session
            .tool_result_text("toolu_tool")
            .contains("edits[].path"),
        "the model is told which location triggered it"
    );

    mur()
        .args(["trace", "show", session.trace_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("protected-path refusals: 1"))
        .stdout(predicate::str::contains(PROTECTED_FILE))
        .stdout(predicate::str::contains("edits[].path"));
}

/// A declared destination adds coverage the key tables do not have: the same input is refused for
/// the tool that declared it and dispatched for the tool that did not.
#[test]
fn a_declared_destination_adds_coverage_the_key_names_do_not_have() {
    if common::skip_without_host_support(
        "a_declared_destination_adds_coverage_the_key_names_do_not_have",
    ) {
        return;
    }
    let declared = run_session(
        one_tool_call("renderer", json!({"sink": PROTECTED_FILE})),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("renderer", Some(RENDERER_SCHEMA))],
        },
        |_| {},
    );

    assert!(!declared.marker_exists(), "the tool must never be invoked");
    let signal = declared.refusal()["signal"].as_str().unwrap().to_string();
    assert!(signal.contains("sink"), "the signal names it: {signal}");

    let undeclared = run_session(
        one_tool_call("renderer", json!({"sink": PROTECTED_FILE})),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("renderer", None)],
        },
        |_| {},
    );

    assert!(
        undeclared.events("protected_path_denied").is_empty(),
        "a path with no content beside it is a read:\n{}",
        undeclared.trace_raw
    );
    assert!(undeclared.marker_exists(), "the tool ran");
}

/// No annotation, at any location, suppresses a refusal: an opaque sibling does not shelter a
/// declared destination, and `murmur-opaque` on a string is ignored.
#[test]
fn no_annotation_suppresses_a_refusal() {
    if common::skip_without_host_support("no_annotation_suppresses_a_refusal") {
        return;
    }
    let sneaky = run_session(
        one_tool_call(
            "sneaky",
            json!({
                "path": PROTECTED_FILE,
                "content": "x",
                "body": {"file": PROTECTED_FILE, "text": "note"}
            }),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("sneaky", Some(SNEAKY_SCHEMA))],
        },
        |_| {},
    );

    assert!(!sneaky.marker_exists(), "the tool must never be invoked");
    assert_eq!(sneaky.protected_file_contents(), PROTECTED_CONTENTS);
    let signal = sneaky.refusal()["signal"].as_str().unwrap().to_string();
    assert!(
        signal.contains("path"),
        "the declared destination is named: {signal}"
    );

    let mislabeled = run_session(
        one_tool_call(
            "mislabeled",
            json!({"path": PROTECTED_FILE, "content": "x"}),
        ),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("mislabeled", Some(MISLABELED_SCHEMA))],
        },
        |_| {},
    );

    assert!(
        !mislabeled.marker_exists(),
        "the tool must never be invoked"
    );
    assert_eq!(mislabeled.protected_file_contents(), PROTECTED_CONTENTS);
    let signal = mislabeled.refusal()["signal"].as_str().unwrap().to_string();
    assert!(
        signal.contains("path") && signal.contains("content"),
        "murmur-opaque on a string is ignored and the heuristic runs: {signal}"
    );
}

/// An absolute path resolves to the same rule a relative one does, and the record reports the
/// workdir-relative form rather than the absolute spelling the model typed.
///
/// Staged with an explicit accessible workdir, because with no override the runtime mints a
/// random session directory and the test could not name a file inside it before scripting the
/// driver.
#[test]
fn an_absolute_path_resolves_to_the_same_rule() {
    if common::skip_without_host_support("an_absolute_path_resolves_to_the_same_rule") {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let accessible = project.path().to_path_buf();

    fs::create_dir_all(accessible.join("tests")).unwrap();
    fs::write(accessible.join(PROTECTED_FILE), PROTECTED_CONTENTS).unwrap();
    let absolute = accessible.join(PROTECTED_FILE);

    let server = common::ScriptedServer::start(one_tool_call(
        "writer",
        json!({"destination": absolute.to_str().unwrap()}),
    ));

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();
    let tool_artifact = common::create_native_artifact(
        artifact_dir.path(),
        "writer",
        "0.1.0",
        &writer_tool_script(),
        Some("Fixture writer tool"),
        None,
    );
    common::publish_local(&home, &tool_artifact).success();

    let manifest_path = create_manifest(
        project.path(),
        &server.endpoint,
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("writer", None)],
        },
    );
    let staged = common::stage_agent_session_with_workdir(
        &home,
        project.path(),
        &manifest_path,
        &accessible,
    );
    let session_dir = staged.workdir.clone();
    fs::write(session_dir.join("task.md"), "Do the thing.").unwrap();

    launch_session(staged, |_| {}).expect("the session must launch");

    let trace_raw = fs::read_to_string(session_dir.join("trace.jsonl")).unwrap_or_default();
    let refusals: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["event_type"] == "protected_path_denied")
        .collect();

    assert_eq!(
        refusals.len(),
        1,
        "an absolute path inside the workdir is covered:\n{trace_raw}"
    );
    assert_eq!(refusals[0]["rule"], "tests");
    assert_eq!(
        refusals[0]["path"], PROTECTED_FILE,
        "the workdir-relative form is recorded, not the absolute spelling"
    );
    assert!(
        !accessible.join(MARKER).exists(),
        "the tool must never be invoked"
    );
}

/// A path outside the workdir is not this mechanism's business: nothing is recorded, and whatever
/// else happens to the call is unchanged by the read-only declaration.
#[test]
fn a_path_outside_the_workdir_records_nothing() {
    if common::skip_without_host_support("a_path_outside_the_workdir_records_nothing") {
        return;
    }
    let session = run_session(
        one_tool_call("writer", json!({"destination": "../elsewhere/x.py"})),
        &Capsule {
            read_only: &["tests"],
            shell_allow: &["bash"],
            tools: &[("writer", None)],
        },
        |_| {},
    );

    assert!(
        session.events("protected_path_denied").is_empty(),
        "outside the workdir is the preopen's and the kernel's business:\n{}",
        session.trace_raw
    );
}

/// An allowlisted interpreter's own file I/O is not analysed: the call is dispatched, not
/// refused, and nothing is recorded.
///
/// Whether the interpreter's write then lands is a different mechanism's answer — on a
/// Landlock-enforcing host it may not even reach its own stdlib — so what is asserted here is
/// only that the manifest check stayed out of the way. A write that does land is pinned by
/// [`a_write_the_analyser_cannot_read_is_not_refused_and_lands`] below, and staging says so with
/// `W-SEC-017` (see [`staging_warns_that_read_only_is_advisory_for_an_interpreter`]).
#[test]
fn an_allowlisted_interpreter_is_not_analysed() {
    if common::skip_without_host_support("an_allowlisted_interpreter_is_not_analysed") {
        return;
    }
    if which("python3").is_none() {
        eprintln!("[SKIP-HOST] an_allowlisted_interpreter_is_not_analysed: no python3 on PATH");
        return;
    }
    let session = run_session(
        one_shell_call(
            "python3",
            &format!("-c \"open('{PROTECTED_FILE}','w').write('x')\""),
        ),
        &capsule(&["tests"], &["bash", "python3"]),
        |_| {},
    );

    assert!(
        session.events("protected_path_denied").is_empty(),
        "the analyser does not flag a write it cannot see:\n{}",
        session.trace_raw
    );
    assert_eq!(
        session.events("shell").len(),
        1,
        "the call was dispatched, not refused:\n{}",
        session.trace_raw
    );
}

/// The honesty bar, with the write actually observed: a shell construct the analyser cannot read
/// is not flagged, and the protected file changes.
///
/// This exists so nobody later closes the gap with a larger string table and claims the boundary
/// moved. Do not delete it and do not "fix" it — the answer to this gap is the kernel-backing
/// layer, and until that lands `W-SEC-017` is what says so.
#[test]
fn a_write_the_analyser_cannot_read_is_not_refused_and_lands() {
    if common::skip_without_host_support(
        "a_write_the_analyser_cannot_read_is_not_refused_and_lands",
    ) {
        return;
    }
    let session = run_session(
        // The redirection is inside a quoted argument, so nothing in the argv or the script text
        // names it: `eval` is not in the write-verb table, and the shell builds the write itself.
        one_shell_call("bash", &format!("eval \"echo x > {PROTECTED_FILE}\"")),
        &capsule(&["tests"], &["bash"]),
        |_| {},
    );

    assert!(
        session.events("protected_path_denied").is_empty(),
        "the analyser flags only what it can positively identify:\n{}",
        session.trace_raw
    );
    assert_eq!(
        session.protected_file_contents(),
        "x\n",
        "the write the analyser could not read landed"
    );
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}

// ── Staging refusals and warnings, through the `mur` binary ──────────────────

fn mur() -> Command {
    Command::cargo_bin("mur").unwrap()
}

/// A script capsule declaring only capabilities — enough to reach staging, which is where both
/// the `E-CAP-012` refusal and the `W-SEC-017` warning are decided.
///
/// An empty `shell_allow` omits the `shell:` block rather than writing an empty one, which is what
/// keeps a case that has nothing to do with shell off the subprocess-capable-host gates: a capsule
/// that can spawn refuses at `E-CAP-005`/`E-RUN-012` on a host without a network namespace or a
/// delegated cgroup scope, and that refusal comes before anything this file asserts on.
fn staging_project(read_only: &[&str], shell_allow: &[&str]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let shell_yaml: String = if shell_allow.is_empty() {
        String::new()
    } else {
        let allowed: String = shell_allow
            .iter()
            .map(|binary| format!("      - {binary}\n"))
            .collect();
        format!("  shell:\n    allow:\n{allowed}")
    };
    let read_only_yaml: String = read_only
        .iter()
        .map(|entry| format!("      - \"{entry}\"\n"))
        .collect();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: staging-capsule\n\
             version: 0.1.0\n\
             capabilities:\n\
             {shell_yaml}\
             \x20 filesystem:\n\
             \x20   read_only:\n\
             {read_only_yaml}"
        ),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    project
}

/// A `read_only` entry that cannot be a workdir subtree refuses the launch with `E-CAP-012`,
/// naming the entry and the rule it broke, before a session directory exists.
///
/// Declares no shell allowlist: the refusal has nothing to do with shell, and a capsule that can
/// spawn is gated on a network namespace and a delegated cgroup scope this case does not need.
#[test]
fn a_malformed_read_only_entry_refuses_the_launch() {
    for (entry, rule) in [
        ("/etc", "must be relative to the workdir"),
        ("../outside", "cannot escape the workdir via '..'"),
        ("tests/../../outside", "cannot escape the workdir via '..'"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let project = staging_project(&[entry], &[]);

        mur()
            .env("HOME", home.path())
            .env_remove("NEXUS_API_KEY")
            .args([
                "run",
                "--manifest",
                project.path().join("murmur.yaml").to_str().unwrap(),
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-012]"))
            .stderr(predicate::str::contains(entry))
            .stderr(predicate::str::contains(rule));

        assert!(
            !home.path().join(".murmur/sessions").exists(),
            "a refused launch creates no session directory"
        );
    }
}

/// Declaring `read_only` alongside an allowlisted interpreter warns, once per interpreter, that
/// the declaration is advisory for that binary — and names the binary.
///
/// The shell allowlist is what the warning is about, so this one cannot drop it: staging refuses a
/// spawn-capable capsule before it warns, and on a host without a network namespace or a delegated
/// cgroup scope that refusal is all there is to see.
#[test]
fn staging_warns_that_read_only_is_advisory_for_an_interpreter() {
    if common::skip_without_host_support(
        "staging_warns_that_read_only_is_advisory_for_an_interpreter",
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let project = staging_project(&["tests"], &["bash", "python3"]);

    mur()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
        ])
        .assert()
        .stderr(predicate::str::contains("warning[W-SEC-017]"))
        .stderr(predicate::str::contains("'python3'"))
        .stderr(predicate::str::contains("advisory for that binary"));
}

/// A capsule that declares nothing read-only warns about nothing.
///
/// Keeps the shell allowlist, because "an allowlisted interpreter and no `read_only`" is the
/// pairing under test — which puts it behind the same host gates as the warning case above. Guarded
/// rather than left to pass on its own: an assertion that something is absent is satisfied by a
/// staging refusal that printed nothing at all.
#[test]
fn no_declaration_means_no_warning() {
    if common::skip_without_host_support("no_declaration_means_no_warning") {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: staging-capsule\nversion: 0.1.0\ncapabilities:\n  shell:\n    allow:\n      - bash\n",
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    mur()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
        ])
        .assert()
        .stderr(predicate::str::contains("W-SEC-017").not());
}

/// A script capsule that installs one native tool, so staging has a tool schema to judge.
///
/// The tool is published into `home` first: `mur run` resolves the artifact out of the local
/// store the same way a real capsule does.
fn staging_project_with_tool(
    home: &tempfile::TempDir,
    artifact_dir: &Path,
    read_only: &[&str],
    tool: &str,
    schema: Option<&str>,
) -> tempfile::TempDir {
    let artifact = common::create_native_artifact(
        artifact_dir,
        tool,
        "0.1.0",
        &writer_tool_script(),
        Some("Fixture writer tool"),
        schema,
    );
    common::publish_local(home, &artifact).success();

    let project = tempfile::tempdir().unwrap();
    let read_only_yaml: String = read_only
        .iter()
        .map(|entry| format!("      - \"{entry}\"\n"))
        .collect();
    let capabilities = if read_only.is_empty() {
        String::new()
    } else {
        format!("capabilities:\n  filesystem:\n    read_only:\n{read_only_yaml}")
    };
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: staging-capsule\n\
             version: 0.1.0\n\
             artifacts:\n\
             \x20 - name: {tool}\n\
             \x20   version: 0.1.0\n\
             \x20   runtime: tool\n\
             {capabilities}"
        ),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    project
}

/// A path-shaped tool schema with no annotation is judged by key name, and staging says so —
/// naming the tool and the property. The same schema with the annotation says nothing.
#[test]
fn staging_warns_that_an_unannotated_tool_schema_is_judged_by_key_name() {
    if common::skip_without_host_support(
        "staging_warns_that_an_unannotated_tool_schema_is_judged_by_key_name",
    ) {
        return;
    }
    let unannotated = r#"{"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}}}"#;
    let annotated = r#"{"type":"object","properties":{"file_path":{"type":"string","format":"murmur-destination"},"content":{"type":"string"}}}"#;

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = staging_project_with_tool(
        &home,
        artifact_dir.path(),
        &["tests"],
        "guessed-tool",
        Some(unannotated),
    );
    mur()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
        ])
        .assert()
        .stderr(predicate::str::contains("warning[W-SEC-018]"))
        .stderr(predicate::str::contains("'guessed-tool'"))
        .stderr(predicate::str::contains("'file_path'"));

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = staging_project_with_tool(
        &home,
        artifact_dir.path(),
        &["tests"],
        "declared-tool",
        Some(annotated),
    );
    mur()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
        ])
        .assert()
        .stderr(predicate::str::contains("W-SEC-018").not());
}

/// A capsule that declares nothing read-only reads no tool schema, so it warns about nothing.
#[test]
fn a_capsule_without_read_only_gets_no_schema_warning() {
    if common::skip_without_host_support("a_capsule_without_read_only_gets_no_schema_warning") {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = staging_project_with_tool(
        &home,
        artifact_dir.path(),
        &[],
        "guessed-tool",
        Some(r#"{"type":"object","properties":{"file_path":{"type":"string"}}}"#),
    );

    mur()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
        ])
        .assert()
        .stderr(predicate::str::contains("W-SEC-018").not());
}

/// The refusal is readable in `mur trace show`: its own line under its own heading, naming the
/// path and the rule, with the count beside it.
#[test]
fn mur_trace_show_renders_the_refusal_and_its_count() {
    if common::skip_without_host_support("mur_trace_show_renders_the_refusal_and_its_count") {
        return;
    }
    let session = run_session(
        one_shell_call("bash", &format!("echo broken > {PROTECTED_FILE}")),
        &capsule(&["tests"], &["bash"]),
        |_| {},
    );

    mur()
        .args(["trace", "show", session.trace_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Protected paths"))
        .stdout(predicate::str::contains("protected-path refusals: 1"))
        .stdout(predicate::str::contains(PROTECTED_FILE))
        .stdout(predicate::str::contains("rule tests"));
}
