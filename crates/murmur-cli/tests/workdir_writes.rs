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

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// One session's outcome: everything left in the accessible workdir, and the declaration the run
/// itself published.
struct Run {
    accessible: PathBuf,
    session_id: String,
    runtime_writes: Vec<Value>,
    /// The session's records directory, under the scratch home kept beside it.
    records_dir: PathBuf,
    /// Every line of the run's `trace.jsonl`.
    events: Vec<Value>,
    /// Every request the scripted driver endpoint received, in order.
    requests: Vec<Value>,
    _home: tempfile::TempDir,
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

    /// The run's trace lines of `event_type`.
    fn events_of_type(&self, event_type: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// The work folder holds nothing under `logs`, and the run's declaration names no `logs`
    /// row: every shell log is a record.
    fn assert_no_logs_in_the_work_folder(&self) {
        assert!(
            !self.accessible.join("logs").exists(),
            "a shell log was written into the work folder"
        );
        for write in &self.runtime_writes {
            assert!(
                !write["path"].as_str().unwrap().starts_with("logs"),
                "the declaration still names a logs row: {write}"
            );
        }
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

/// A command whose output passes the 16 KiB cap: its full output goes to the records directory,
/// the result the model reads names it by its absolute path, and the work folder is left exactly
/// as the capsule left it.
#[test]
fn a_truncated_command_leaves_its_log_in_the_records_directory() {
    if common::skip_without_host_support(
        "a_truncated_command_leaves_its_log_in_the_records_directory",
    ) {
        return;
    }
    // A builtin loop: `bash` is the one binary declared, and on a kernel-enforcement host
    // nothing else is executable.
    let run = run_session(vec![
        bash_response(
            "msg_1",
            "toolu_seq",
            "for ((i = 1; i <= 20000; i++)); do echo $i; done",
        ),
        text_response("msg_2", "done"),
    ]);

    run.assert_no_logs_in_the_work_folder();
    assert_eq!(
        run.left_after_subtracting_the_declaration(),
        BTreeSet::new(),
        "the capsule wrote nothing, and the declaration must account for everything else"
    );

    let result = common::tool_result_text(&run.requests, "toolu_seq")
        .expect("the next request carries the seq result");
    let prefix = format!(
        "Full output written to {}/logs/shell-",
        run.records_dir.display()
    );
    let path = result
        .lines()
        .find_map(|line| {
            line.find(&prefix)
                .map(|at| line[at + "Full output written to ".len()..].trim_end())
        })
        .unwrap_or_else(|| panic!("no `{prefix}` line in the tool result:\n{result}"));
    let log = fs::read_to_string(path).expect("the named log is on disk");
    assert!(
        log.lines().any(|line| line == "20000"),
        "the log holds the full output"
    );
}

/// A command demoted past `lifecycle.shell_grace_secs` that finishes while the session is alive:
/// its log goes to the records directory, the trace names it by its absolute path, and the work
/// folder is left exactly as the capsule left it.
///
/// Every response is held three seconds, so the command is demoted at one second, exits a second
/// later and is on disk before the end-turn response ends the session.
#[test]
fn a_demoted_command_leaves_its_log_in_the_records_directory() {
    if common::skip_without_host_support(
        "a_demoted_command_leaves_its_log_in_the_records_directory",
    ) {
        return;
    }
    let run = run_session_with(
        vec![
            bash_response(
                "msg_1",
                "toolu_build",
                "sleep 2; for ((i = 1; i <= 50; i++)); do echo $i; done",
            ),
            text_response("msg_2", "build started"),
        ],
        std::time::Duration::from_secs(3),
        &["sleep"],
        "lifecycle:\n  shell_grace_secs: 1\n",
    );

    run.assert_no_logs_in_the_work_folder();
    assert_eq!(
        run.left_after_subtracting_the_declaration(),
        BTreeSet::new(),
        "the capsule wrote nothing, and the declaration must account for everything else"
    );

    let detached = run.events_of_type("shell_detached");
    assert_eq!(detached.len(), 1, "the command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap();
    let output_path = run
        .events_of_type("shell_completed")
        .into_iter()
        .chain(run.events_of_type("shell_abandoned"))
        .find(|event| event["work_id"] == work_id)
        .and_then(|event| event["output_path"].as_str())
        .unwrap_or_else(|| {
            panic!(
                "no shell_completed or shell_abandoned line names an output path for {work_id}: \
                 {:?}",
                run.events
            )
        });
    let expected = run.records_dir.join("logs").join(format!("{work_id}.log"));
    assert_eq!(Path::new(output_path), expected);
    let log = fs::read_to_string(&expected).expect("the named log is on disk");
    assert!(log.lines().any(|line| line == "50"), "{log}");
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
        &run.accessible,
    )
    .expect("the scratch home can hold ~/.murmur");
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
    run_session_with(responses, std::time::Duration::ZERO, &[], "")
}

/// [`run_session`], holding each scripted response for `delay`, allowing `extra_shell` beside
/// `bash`, and appending `lifecycle_yaml` to the manifest verbatim.
fn run_session_with(
    responses: Vec<String>,
    delay: std::time::Duration,
    extra_shell: &[&str],
    lifecycle_yaml: &str,
) -> Run {
    let server = ScriptedServer::start_with_delay(responses, delay);
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

    let manifest_path = write_manifest(
        project.path(),
        &server.endpoint,
        extra_shell,
        lifecycle_yaml,
    );
    let staged = common::stage_agent_session_with_workdir(
        &home,
        project.path(),
        &manifest_path,
        accessible.path(),
    );
    let session_id = staged.session_id.clone();
    let records_dir = staged.records_dir.clone();
    // The way `mur run --task` delivers it: into the runtime directory, so the workdir holds
    // nothing the operator put there.
    staged.write_task_input(b"Say done.").unwrap();

    launch_session(staged, |_| {}).expect("the session must launch");

    let trace = fs::read_to_string(records_dir.join("trace.jsonl")).unwrap();
    let events: Vec<Value> = trace
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect();
    let start = events
        .iter()
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
        records_dir,
        events,
        requests: server.requests(),
        _home: home,
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

fn write_manifest(
    project_dir: &Path,
    endpoint: &str,
    extra_shell: &[&str],
    lifecycle_yaml: &str,
) -> PathBuf {
    let manifest = project_dir.join("murmur.yaml");
    let extra_shell_yaml: String = extra_shell
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect();
    fs::write(
        &manifest,
        format!(
            "name: workdir-writes-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
             endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\n{extra_shell_yaml}inference:\n  \
             transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
             {lifecycle_yaml}"
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
