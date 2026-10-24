//! The property the `runtime_writes` declaration exists for: a consumer for whom the work folder
//! is the deliverable can subtract what the runtime wrote and be left with exactly what the
//! capsule changed — which, the runtime writing nothing there of its own, is everything in it.
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

use common::{
    hook_wat::{compaction_hook_wasm, create_hook_zip},
    ScriptedServer,
};

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
    /// Every path in the work folder, relative and slash-separated, with each declared entry
    /// subtracted — a `directory` row as a prefix, a `file` row exactly, and either one's parent
    /// directories with it.
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
            let declared = write["path"].as_str().unwrap().to_string();
            if path == declared {
                return true;
            }
            // A parent of a declared path is the runtime's too: `git status
            // --untracked-files=all` — the shape of subtraction this stands in for — reports files
            // rather than the directories above them.
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

    /// The declaration names exactly the two paths a capsule asks for, and no row is
    /// unconditional: a session that redeems no handle and is no script capsule leaves the work
    /// folder as it found it.
    fn assert_the_declaration_is_peer_in_and_a_script_capsules_out(&self) {
        assert_eq!(
            self.runtime_writes,
            vec![
                json!({"path": "peer-in", "kind": "directory", "condition": "peer-fetch"}),
                json!({"path": "out", "kind": "directory", "condition": "script-session"}),
            ]
        );
    }
}

/// A capsule that answers and stops: the runtime leaves nothing in the work folder.
#[test]
fn a_capsule_that_wrote_nothing_leaves_nothing_the_declaration_does_not_name() {
    if common::skip_without_host_support(
        "a_capsule_that_wrote_nothing_leaves_nothing_the_declaration_does_not_name",
    ) {
        return;
    }
    let run = run_session(vec![text_response("msg_1", "done")]);

    run.assert_the_declaration_is_peer_in_and_a_script_capsules_out();
    assert_eq!(walk(&run.accessible, &run.accessible), Vec::<String>::new());
    assert_eq!(
        fs::read_to_string(run.records_dir.join("out/result.txt")).unwrap(),
        "done"
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

    // The shell call's home folder is in the records directory, and nothing of the runtime's is
    // in the work folder.
    assert!(run.records_dir.join("home").is_dir());
    assert!(!run.accessible.join(".murmur").exists());
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
            "a declared path carries no session id: {write}"
        );
    }
}

// ── The work folder holds only what the agent changed ────────────────────────

const COMPACTOR: &str = "local/compactor";
const COMPACTION_SUMMARY: &str = "the probe, the notes and the generated folder are in place";
const FINAL_ANSWER: &str = "All done.";

/// What one [`run_agent`] left: the work folder, the run's own records directory, its trace and
/// how the launch ended.
struct AgentRun {
    work_folder: PathBuf,
    records_dir: PathBuf,
    events: Vec<Value>,
    launch_error: Option<String>,
    _dirs: Vec<tempfile::TempDir>,
}

impl AgentRun {
    fn has_event(&self, event_type: &str) -> bool {
        self.events
            .iter()
            .any(|event| event["event_type"] == event_type)
    }
}

/// An anthropic response reporting `usage`: a call of `tool` with `input`, or the end of the turn
/// with `text` when `tool` is `None`.
fn scripted(id: &str, tool: Option<(&str, Value)>, text: &str, usage: (u64, u64)) -> String {
    let (content, stop_reason) = match tool {
        Some((name, input)) => (
            json!([{"type": "tool_use", "id": format!("toolu_{id}"), "name": name, "input": input}]),
            "tool_use",
        ),
        None => (json!([{"type": "text", "text": text}]), "end_turn"),
    };
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": usage.0, "output_tokens": usage.1}
    })
    .to_string()
}

/// The scripted model of the acceptance runs: `command` through `bash`, the fixture native tool's
/// `create_dir` of `generated` with label `x`, a call whose reported usage crosses the
/// compaction trigger (400 of a 4000-token input budget), and the end of the turn.
fn acceptance_responses(command: &str) -> Vec<String> {
    vec![
        scripted(
            "msg_1",
            Some(("bash", json!({"command": command}))),
            "",
            (10, 5),
        ),
        scripted(
            "msg_2",
            Some((
                common::FIXTURE_NATIVE_TOOL_NAME,
                json!({"operation": "create_dir", "path": "generated", "label": "x"}),
            )),
            "",
            (10, 5),
        ),
        scripted(
            "msg_3",
            Some(("bash", json!({"command": "true"}))),
            "",
            (520, 20),
        ),
        scripted("msg_4", None, FINAL_ANSWER, (30, 5)),
    ]
}

/// Launch an agent session in-process with `bash` and `extra_shell` allowed, the fixture native
/// tool installed, an `on-compaction` hook that replaces the context, compaction at 10% of a
/// 4000-token input budget (a 12192-token window less the fixture driver's 8192 output cap) with
/// `dump_summaries`, `extra_capabilities` appended under
/// `capabilities:`, and `workdir` as its `--workdir` when given. `None` when the native tool
/// fixture cannot be built on this host.
fn run_agent(
    responses: Vec<String>,
    delay: std::time::Duration,
    workdir: Option<&Path>,
    extra_shell: &[&str],
    extra_capabilities: &str,
) -> Option<AgentRun> {
    let binary = common::fixture_native_tool_binary()?;
    let server = ScriptedServer::start_with_delay(responses, delay);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let tool = common::create_native_tool_zip(
        artifact_dir.path(),
        common::FIXTURE_NATIVE_TOOL_NAME,
        "0.1.0",
        &fs::read(common::fixture_native_tool_manifest()).unwrap(),
        &binary,
    );
    common::publish_local(&home, &tool).success();
    let hook = create_hook_zip(
        artifact_dir.path(),
        COMPACTOR,
        "on-compaction",
        "replace-context",
        &compaction_hook_wasm(COMPACTION_SUMMARY),
    );
    common::publish_local(&home, &hook).success();

    let endpoint = &server.endpoint;
    let tool_name = common::FIXTURE_NATIVE_TOOL_NAME;
    let extra_shell_yaml: String = extra_shell
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        format!(
            "name: workdir-writes-agent\nversion: 0.1.0\ncontext:\n  max_tokens: 12192\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    \
             runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n  \
             - name: local/{tool_name}\n    version: 0.1.0\n    runtime: tool\n  \
             - name: {COMPACTOR}\n    version: 0.1.0\n    runtime: hook\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n      \
             - bash\n{extra_shell_yaml}{extra_capabilities}inference:\n  transport: http\n  \
             model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n  compaction:\n    \
             threshold: 0.1\n    dump_summaries: true\nlifecycle:\n  shell_grace_secs: 30\n"
        ),
    )
    .unwrap();

    let staged = match workdir {
        Some(workdir) => {
            common::stage_agent_session_with_workdir(&home, project.path(), &manifest_path, workdir)
        }
        None => common::stage_agent_session(&home, project.path(), &manifest_path),
    };
    let work_folder = staged.accessible_workdir.clone();
    // The run's own record of where its records are: staging decided it, and the launch reports
    // the same directory when it returns.
    let records_dir = staged.records_dir.clone();
    staged
        .write_task_input(b"Probe, write the notes and generate the folder.")
        .unwrap();

    let launch_error = match launch_session(staged, |_| {}) {
        Ok(launched) => {
            assert_eq!(launched.records_dir, records_dir);
            assert_eq!(launched.workdir, work_folder);
            None
        }
        Err(error) => Some(error.to_string()),
    };
    let events = fs::read_to_string(records_dir.join("trace.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Some(AgentRun {
        work_folder,
        records_dir,
        events,
        launch_error,
        _dirs: vec![home, artifact_dir, project],
    })
}

/// `git` in `repo` with a fixed identity, its stdout.
fn git(repo: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=murmur-test",
            "-c",
            "user.email=murmur-test@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// The records directory holds the shell command's `$HOME` file, the final answer and the one
/// committed compaction's summary, and the trace records that compaction.
fn assert_the_records_hold_home_results_and_summaries(run: &AgentRun) {
    assert_eq!(run.launch_error, None, "the session completes");
    assert_eq!(
        fs::read_to_string(run.records_dir.join("home/probe")).unwrap(),
        "probe\n"
    );
    assert_eq!(
        fs::read_to_string(run.records_dir.join("out/result.txt")).unwrap(),
        FINAL_ANSWER
    );
    let summaries =
        fs::read_to_string(run.records_dir.join("out/compaction-summaries.jsonl")).unwrap();
    assert_eq!(summaries.lines().count(), 1, "{summaries}");
    assert!(summaries.contains(COMPACTION_SUMMARY), "{summaries}");
    assert!(run.has_event("compaction"), "{:#?}", run.events);
}

/// The acceptance check: a git work folder handed to `--workdir`, a session that runs a shell
/// command, calls a native tool and compacts, and `git status` lists only what the agent changed.
#[test]
fn a_git_work_folder_shows_only_what_the_agent_changed() {
    if common::skip_without_host_support("a_git_work_folder_shows_only_what_the_agent_changed") {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "--quiet"]);
    fs::write(repo.path().join("README.md"), "# repo\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "--quiet", "-m", "init"]);

    let Some(run) = run_agent(
        acceptance_responses(
            "echo probe > \"$HOME/probe\"; echo agent > notes.txt; echo more >> README.md",
        ),
        std::time::Duration::ZERO,
        Some(repo.path()),
        &[],
        "",
    ) else {
        eprintln!("SKIP: the native tool fixture could not be built on this host");
        return;
    };

    let status = git(
        repo.path(),
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let mut lines: Vec<&str> = status.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [" M README.md", "?? generated/label.txt", "?? notes.txt"],
        "{status}"
    );
    assert!(!repo.path().join(".murmur").exists());
    assert_the_records_hold_home_results_and_summaries(&run);
}

/// The same session without `--workdir`: the work folder the runtime created holds exactly what
/// the agent wrote.
#[test]
fn a_default_work_folder_holds_only_what_the_agent_wrote() {
    if common::skip_without_host_support("a_default_work_folder_holds_only_what_the_agent_wrote") {
        return;
    }
    let Some(run) = run_agent(
        acceptance_responses("echo probe > \"$HOME/probe\"; echo agent > notes.txt"),
        std::time::Duration::ZERO,
        None,
        &[],
        "",
    ) else {
        eprintln!("SKIP: the native tool fixture could not be built on this host");
        return;
    };

    let mut files = walk(&run.work_folder, &run.work_folder);
    files.sort_unstable();
    assert_eq!(files, ["generated", "generated/label.txt", "notes.txt"]);
    assert_the_records_hold_home_results_and_summaries(&run);
}

/// Under `--workdir` the ceiling covers the operator's folder: a command writing 2 MB into it
/// past a 1 MiB `workdir_max_bytes` ends the session with `E-RUN-013`, and the breach's
/// bootstrap log line names the work folder, `tmp/` and `home/` it measured.
#[test]
fn a_work_folder_past_its_ceiling_ends_the_session_under_workdir() {
    if common::skip_without_host_support(
        "a_work_folder_past_its_ceiling_ends_the_session_under_workdir",
    ) {
        return;
    }
    let workdir = tempfile::tempdir().unwrap();
    // A builtin loop: on a kernel-enforcement host only the allowed binaries are executable.
    let write_2mb = "block=$(printf '%01024d' 0); for ((i = 0; i < 2048; i++)); do \
                     printf '%s' \"$block\"; done > big.bin";
    let responses = vec![
        scripted(
            "msg_1",
            Some(("bash", json!({"command": write_2mb}))),
            "",
            (10, 5),
        ),
        // Longer than the guard's 10 s check interval.
        scripted(
            "msg_2",
            Some(("bash", json!({"command": "sleep 12"}))),
            "",
            (10, 5),
        ),
        scripted(
            "msg_3",
            Some(("bash", json!({"command": "true"}))),
            "",
            (10, 5),
        ),
        scripted("msg_4", None, "never reached", (10, 5)),
    ];
    let Some(run) = run_agent(
        responses,
        std::time::Duration::ZERO,
        Some(workdir.path()),
        &["sleep"],
        "  resources:\n    workdir_max_bytes: 1048576\n",
    ) else {
        eprintln!("SKIP: the native tool fixture could not be built on this host");
        return;
    };

    let error = run
        .launch_error
        .as_deref()
        .expect("the breach ends the session");
    assert!(
        error.contains("past the 1048576 byte ceiling (capabilities.resources.workdir_max_bytes)"),
        "{error}"
    );
    assert!(error.contains("work folder, $HOME and $TMPDIR"), "{error}");
    let size = fs::metadata(workdir.path().join("big.bin")).unwrap().len();
    assert_eq!(size, 2 * 1024 * 1024);
    let log = fs::read_to_string(run.records_dir.join("logs/bootstrap.log")).unwrap();
    let line = log
        .lines()
        .find(|line| line.contains("measured: "))
        .unwrap_or_else(|| panic!("no breach line names what it measured:\n{log}"));
    for measured in [
        workdir.path().to_path_buf(),
        run.records_dir.join("tmp"),
        run.records_dir.join("home"),
    ] {
        assert!(line.contains(&measured.display().to_string()), "{line}");
    }
    assert!(!workdir.path().join(".murmur").exists());
}

/// Stage an agent session against an explicit work folder — the shape `mur run --workdir` takes —
/// script the driver with `responses`, and read the declaration back out of the run's own trace.
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
