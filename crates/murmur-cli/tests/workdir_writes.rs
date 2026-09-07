//! The property the `runtime_writes` declaration exists for: a consumer for whom the accessible
//! workdir is the deliverable can subtract what the runtime wrote and be left with exactly what
//! the capsule changed.
//!
//! The subtraction here is performed the way that consumer would perform it — from the run's own
//! `trace.jsonl`, with no list written into this file. Anything the runtime leaves behind that the
//! declaration does not name fails these tests, which is the whole point: a hand-maintained
//! exclusion list is what this replaces.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use serde_json::{json, Value};

use common::ScriptedServer;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// One session's outcome: everything left in the accessible workdir, and the declaration the run
/// itself published.
struct Run {
    accessible: PathBuf,
    session_id: String,
    runtime_writes: Vec<Value>,
}

impl Run {
    /// Every path in the accessible workdir, relative and slash-separated, with each declared
    /// accessible-scope entry subtracted — a `directory` row as a prefix, a `file` row exactly,
    /// and either one's parent directories with it.
    ///
    /// The literal `<session-id>` in a declared path is replaced with this run's own id, which
    /// `session_start.session_id` carried on the same line the declaration came from.
    fn left_after_subtracting_the_declaration(&self) -> BTreeSet<String> {
        let mut remaining = BTreeSet::new();
        for entry in walk(&self.accessible, &self.accessible) {
            if !self.is_declared(&entry) {
                remaining.insert(entry);
            }
        }
        remaining
    }

    fn is_declared(&self, path: &str) -> bool {
        self.runtime_writes.iter().any(|write| {
            if write["scope"] != "accessible" {
                return false;
            }
            let declared = write["path"]
                .as_str()
                .unwrap()
                .replace("<session-id>", &self.session_id);
            if path == declared {
                return true;
            }
            // A parent of a declared path is the runtime's too: nothing created `.murmur/` for
            // its own sake, and `git status --untracked-files=all` — the shape of subtraction
            // this stands in for — reports files rather than the directories above them.
            if declared.starts_with(&format!("{path}/")) {
                return true;
            }
            write["kind"] == "directory" && path.starts_with(&format!("{declared}/"))
        })
    }

    /// Every declared row that is in the accessible workdir and claims to be written by every
    /// session must really be there.
    fn assert_every_unconditional_declaration_exists(&self) {
        for write in &self.runtime_writes {
            if write["scope"] != "accessible" || write["condition"] != "always" {
                continue;
            }
            let declared = write["path"]
                .as_str()
                .unwrap()
                .replace("<session-id>", &self.session_id);
            assert!(
                self.accessible.join(&declared).exists(),
                "`{declared}` is declared as written by every session and is not on disk"
            );
        }
    }
}

/// A capsule that answers and stops. Whatever is in the workdir afterwards came from the runtime.
#[test]
fn a_capsule_that_wrote_nothing_leaves_nothing_the_declaration_does_not_name() {
    if common::skip_without_host_support(
        "a_capsule_that_wrote_nothing_leaves_nothing_the_declaration_does_not_name",
    ) {
        return;
    }
    let run = run_session(vec![text_response("msg_1", "done")]);

    run.assert_every_unconditional_declaration_exists();
    assert_eq!(
        run.left_after_subtracting_the_declaration(),
        BTreeSet::new(),
        "everything in the workdir was the runtime's, and the declaration must name all of it"
    );
}

/// The same subtraction over a session whose capsule wrote one file: exactly that file survives.
///
/// This is the half that proves the subtraction is not vacuous — a declaration that swallowed the
/// whole directory would pass the test above and fail this one.
#[test]
fn a_capsule_that_wrote_one_file_leaves_exactly_that_file() {
    if common::skip_without_host_support("a_capsule_that_wrote_one_file_leaves_exactly_that_file") {
        return;
    }
    let run = run_session(vec![
        bash_response("msg_1", "toolu_write", "echo hello > hello.txt"),
        text_response("msg_2", "done"),
    ]);

    assert_eq!(
        fs::read_to_string(run.accessible.join("hello.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        run.left_after_subtracting_the_declaration(),
        BTreeSet::from(["hello.txt".to_string()]),
        "the capsule's own file is the only thing the declaration does not account for"
    );

    // The synthetic home the shell call materialised, where the declaration says it is. Spelled
    // out beside the subtraction because the subtraction alone cannot tell a directory that moved
    // from one that was never created.
    assert!(run
        .accessible
        .join(".murmur")
        .join(&run.session_id)
        .join(".capsule-home")
        .is_dir());
    assert!(!run.accessible.join(".capsule-home").exists());
}

/// The declaration a run publishes is the declaration `mur run --explain-scope` prints, so a
/// consumer can capture it before the run and compare it to what the run reports.
#[test]
fn the_trace_carries_the_same_declaration_explain_scope_prints() {
    if common::skip_without_host_support(
        "the_trace_carries_the_same_declaration_explain_scope_prints",
    ) {
        return;
    }
    let run = run_session(vec![text_response("msg_1", "done")]);

    let declared = capsule_runtime::explain_scope(
        &capsule_runtime::CapabilityPolicy::default(),
        murmur_artifact::ContainmentClass::Advisory,
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        capsule_runtime::IoMaxReport::default(),
    );
    assert_eq!(
        serde_json::to_value(&declared.runtime_writes).unwrap(),
        Value::Array(run.runtime_writes.clone()),
        "the trace must carry the report verbatim, not a second derivation of it"
    );
    for write in &run.runtime_writes {
        assert!(
            !write["path"].as_str().unwrap().contains(&run.session_id),
            "a declared path carries the literal <session-id>, never a real id: {write}"
        );
    }
}

/// Stage an agent session against an explicit accessible workdir — the shape `mur run --workdir`
/// takes, and the only shape in which the two workdirs are different directories — script the
/// driver with `responses`, and read the declaration back out of the run's own trace.
fn run_session(responses: Vec<String>) -> Run {
    let server = ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let accessible = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = write_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session_with_workdir(
        &home,
        project.path(),
        &manifest_path,
        accessible.path(),
    );
    let session_id = staged.session_id.clone();
    let session_dir = staged.workdir.clone();
    fs::write(accessible.path().join("task.md"), "Say done.").unwrap();

    launch_session(staged, |_| {}).expect("the session must launch");

    let trace = fs::read_to_string(session_dir.join("trace.jsonl")).unwrap();
    let start: Value = trace
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "session_start")
        .expect("every session writes a session_start line");
    assert_eq!(start["session_id"], session_id.as_str());

    let runtime_writes = start["effective_grants"]["runtime_writes"]
        .as_array()
        .expect("session_start.effective_grants carries runtime_writes")
        .clone();

    // The temp dir is consumed here rather than dropped: the walk below reads it, and the run's
    // own directory must outlive the assertions made about it.
    let accessible = accessible.keep();
    Run {
        accessible,
        session_id,
        runtime_writes,
    }
}

/// Every file and directory under `root`, as slash-separated paths relative to `base`.
///
/// Directories are listed as well as their contents, so a declared `directory` row is checked as
/// the prefix it claims to be rather than only through what happens to be inside it.
fn walk(base: &Path, root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        found.push(
            path.strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
        );
        if path.is_dir() {
            found.extend(walk(base, &path));
        }
    }
    found
}

fn write_manifest(project_dir: &Path, endpoint: &str) -> PathBuf {
    let manifest = project_dir.join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: workdir-writes-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\ninference:\n  \
             transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  \
             driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    manifest
}

fn text_response(id: &str, text: &str) -> String {
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

fn bash_response(id: &str, tool_use_id: &str, command: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": tool_use_id,
            "name": "bash",
            "input": {"command": command}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}
