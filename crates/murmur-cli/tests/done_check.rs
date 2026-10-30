//! `lifecycle.done_check`: the operator's command decides whether a task's work is done, and a
//! failing run reopens the task with the run's facts, up to the check's own limit.
//!
//! Driven through the real `mur` binary against the anthropic driver fixture and a scripted
//! provider, because the properties are what the provider was sent and what the trace and
//! `out/result.txt` say afterwards. The cancel case launches in-process, as `keep_working.rs`
//! does, so it can reach the capsule's door.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, DoneCheck, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Make the tests pass.";
/// Fails every run.
const ALWAYS_FAILS: &str = "bash -c 'echo still failing; exit 3'";
/// Fails its first run in a work folder and passes every later one. The marker is written by a
/// redirection rather than `touch`: the capsule's containment executes only allowlisted programs.
const FAILS_ONCE: &str = "bash -c 'if [ -f .dc-ran ]; then echo green; exit 0; fi; : > .dc-ran; \
                          echo \"1 test failed: test_parse\"; exit 1'";

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

fn tool_turn(id: &str, name: &str, input: Value) -> String {
    response(
        json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        "tool_use",
    )
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the anthropic driver fixture published and compiled, so a launch's
/// staging loads it rather than compiling it: a deadline counts from `mur run`'s start.
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

/// `lifecycle.done_check` running `command` with `limit`, and `extra` lines inside the block.
fn done_check(command: &str, limit: u32, extra: &str) -> String {
    // A JSON string is a YAML double-quoted scalar.
    format!(
        "  done_check:\n    command: {}\n    limit: {limit}\n{extra}",
        serde_json::to_string(command).unwrap()
    )
}

/// An http capsule on `endpoint` whose shell may run `bash` and `sleep`, which the hanging checks
/// need; `lifecycle` and `inference` are spliced into their blocks.
fn http_manifest(endpoint: &str, lifecycle: &str, inference: &str) -> String {
    format!(
        "name: done-check\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    \
         allow: [bash, sleep]\nlifecycle:\n  task_acceptance: single\n{lifecycle}inference:\n  transport: \
         http\n  model: test-model\n{inference}  driver:\n    artifact: {DRIVER}\n"
    )
}

struct Project {
    dir: TempDir,
    manifest: PathBuf,
}

fn project(manifest_yaml: &str) -> Project {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(&manifest, manifest_yaml).unwrap();
    Project { dir, manifest }
}

struct Run {
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

    /// The session's work folder, from the startup block's `workdir:` line.
    fn workdir(&self) -> PathBuf {
        let line = self
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("workdir: "))
            .unwrap_or_else(|| panic!("no workdir line: {}", self.context()));
        PathBuf::from(line.trim())
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(&self.records_dir().join("trace.jsonl"))
    }

    fn result(&self) -> String {
        fs::read_to_string(self.records_dir().join("out").join("result.txt")).unwrap_or_default()
    }
}

/// `mur run --task … --verbose` on `project`, with `extra` arguments.
fn run(home: &TempDir, project: &Project, extra: &[&str]) -> Run {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MURMUR_A2A_TIMEOUT_SECS", "1")
        .current_dir(project.dir.path())
        .args([
            "run",
            "--manifest",
            project.manifest.to_str().unwrap(),
            "--task",
            TASK,
            "--verbose",
        ])
        .args(extra)
        .timeout(Duration::from_secs(300))
        .output()
        .unwrap();
    Run {
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

fn position(trace: &[Value], event_type: &str) -> usize {
    *positions(trace, event_type)
        .first()
        .unwrap_or_else(|| panic!("no {event_type} in {trace:#?}"))
}

/// The text of a provider request's last message, which must be a user message with one text
/// block.
fn last_user_text(request: &Value) -> String {
    let last = request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .unwrap_or_else(|| panic!("no messages in {request}"));
    assert_eq!(last["role"], "user", "{last}");
    let blocks = last["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1, "{last}");
    assert_eq!(blocks[0]["type"], "text", "{last}");
    blocks[0]["text"].as_str().unwrap().to_string()
}

/// Asserts the `done_check` records' `(outcome, exit_code, reopened, reopens_left)`, in order.
fn assert_runs(trace: &[Value], expected: &[(&str, Value, bool, u32)]) {
    let records = events(trace, "done_check");
    assert_eq!(records.len(), expected.len(), "{trace:#?}");
    for (index, (record, (outcome, exit_code, reopened, left))) in
        records.iter().zip(expected).enumerate()
    {
        assert_eq!(record["number"], index + 1, "{record}");
        assert_eq!(record["outcome"], *outcome, "{record}");
        assert_eq!(record["exit_code"], *exit_code, "{record}");
        assert_eq!(record["reopened"], *reopened, "{record}");
        assert_eq!(record["reopens_left"], *left, "{record}");
    }
}

// ── No block ──────────────────────────────────────────────────────────────────

/// With no `lifecycle.done_check` nothing runs: the task ends at its first turn end.
#[test]
fn without_the_block_nothing_runs() {
    if common::skip_without_host_support("without_the_block_nothing_runs") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("first")]);
    let project = project(&http_manifest(&server.endpoint, "", ""));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 1);
    let trace = run.trace();
    assert!(events(&trace, "done_check").is_empty(), "{trace:#?}");
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert!(task_end.get("done_check").is_none(), "{task_end}");
    assert_eq!(run.result(), "first");
}

// ── Fails once, then met ──────────────────────────────────────────────────────

/// A check that fails once reopens the task once with the run's facts as the next user message,
/// and the task ends `ok` with the check met. Nothing else changes in what the provider is sent.
#[test]
fn a_check_that_fails_once_reopens_once_and_is_met() {
    if common::skip_without_host_support("a_check_that_fails_once_reopens_once_and_is_met") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("one"), end_turn("two")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(FAILS_ONCE, 3, ""),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    let trace = run.trace();
    assert_runs(
        &trace,
        &[
            ("failed", json!(1), true, 2),
            ("passed", json!(0), false, 2),
        ],
    );
    let inferences = positions(&trace, "inference");
    let checks = positions(&trace, "done_check");
    let task_end_at = position(&trace, "task_end");
    assert!(inferences[0] < checks[0] && checks[0] < inferences[1]);
    assert!(inferences[1] < checks[1] && checks[1] < task_end_at);
    assert!(events(&trace, "task_reopened").is_empty(), "{trace:#?}");
    assert!(events(&trace, "shell").is_empty(), "{trace:#?}");
    let record = events(&trace, "done_check")[0];
    assert_eq!(record["command"], FAILS_ONCE);
    assert_eq!(record["stdout_bytes"], "1 test failed: test_parse\n".len());
    assert_eq!(record["turns_remaining"], Value::Null);

    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(task_end["reopen_count"], 0);
    assert_eq!(task_end["turns"], 2);
    assert_eq!(task_end["shell_calls"], 0);
    assert_eq!(
        task_end["done_check"],
        json!({"runs": 2, "reopens": 1, "limit": 3, "met": true, "stopped_by": null})
    );
    assert_eq!(run.result(), "two");

    let feedback = last_user_text(&requests[1]);
    assert_eq!(
        feedback,
        format!(
            "Done check\n<untrusted-content source=done-check>\n$ {FAILS_ONCE}\nExit code: 1\n\
             Stdout:\n1 test failed: test_parse\n\nStderr:\n\n</untrusted-content>"
        )
    );
    assert!(!feedback.contains("The previous attempt was not accepted"));

    // The first request's system prompt is the same with and without the block.
    let control_server = common::ScriptedServer::start(vec![end_turn("first")]);
    let control_project = project_without_check(&control_server);
    let control = run_control(&home, &control_project);
    assert_eq!(
        control.output.status.code(),
        Some(0),
        "{}",
        control.context()
    );
    let system = |request: &Value, endpoint: &str| {
        request["system"]
            .to_string()
            .replace(endpoint, "<endpoint>")
    };
    assert_eq!(
        system(&control_server.requests()[0], &control_server.endpoint),
        system(&requests[0], &server.endpoint),
        "the block adds nothing to the system prompt"
    );
}

fn project_without_check(server: &common::ScriptedServer) -> Project {
    project(&http_manifest(&server.endpoint, "", ""))
}

/// [`run`] under its own name, so a second launch in one case reads as the control it is.
fn run_control(home: &TempDir, project: &Project) -> Run {
    run(home, project, &[])
}

// ── The limit ─────────────────────────────────────────────────────────────────

/// A check that always fails reopens up to its limit, then the task ends `ok` with the last
/// answer and the check unmet.
#[test]
fn a_check_that_always_fails_ends_ok_at_the_limit() {
    if common::skip_without_host_support("a_check_that_always_fails_ends_ok_at_the_limit") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("a"), end_turn("b"), end_turn("c")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(ALWAYS_FAILS, 2, ""),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    assert_eq!(server.requests().len(), 3);
    let trace = run.trace();
    assert_runs(
        &trace,
        &[
            ("failed", json!(3), true, 1),
            ("failed", json!(3), true, 0),
            ("failed", json!(3), false, 0),
        ],
    );
    assert_eq!(
        only(&trace, "task_end")["done_check"],
        json!({"runs": 3, "reopens": 2, "limit": 2, "met": false, "stopped_by": "limit"})
    );
    assert_eq!(only(&trace, "session_end")["exit_status"], "ok");
    assert_eq!(run.result(), "c");
}

/// `limit: 0` runs and records the check at the stop and never reopens.
#[test]
fn limit_zero_runs_the_check_and_never_reopens() {
    if common::skip_without_host_support("limit_zero_runs_the_check_and_never_reopens") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("a")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(ALWAYS_FAILS, 0, ""),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 1);
    let trace = run.trace();
    assert_runs(&trace, &[("failed", json!(3), false, 0)]);
    assert_eq!(
        only(&trace, "task_end")["done_check"],
        json!({"runs": 1, "reopens": 0, "limit": 0, "met": false, "stopped_by": "limit"})
    );
    assert_eq!(run.result(), "a");
}

// ── The turn limit ────────────────────────────────────────────────────────────

/// `max_turns` stops the check: at a turn end with no turn left, and inside a reopened attempt,
/// which ends the task `ok` with the answer that preceded the reopen.
#[test]
fn max_turns_stops_the_done_check() {
    if common::skip_without_host_support("max_turns_stops_the_done_check") {
        return;
    }
    let home = home_with_driver();

    // (a) At the turn end that spends the last turn.
    let server = common::ScriptedServer::start(vec![end_turn("a"), end_turn("b")]);
    let at_turn_end = project(&http_manifest(
        &server.endpoint,
        &done_check(ALWAYS_FAILS, 5, ""),
        "  max_turns: 2\n",
    ));
    let run_a = run(&home, &at_turn_end, &[]);
    assert_eq!(run_a.output.status.code(), Some(0), "{}", run_a.context());
    let trace = run_a.trace();
    assert_runs(
        &trace,
        &[
            ("failed", json!(3), true, 4),
            ("failed", json!(3), false, 4),
        ],
    );
    assert_eq!(events(&trace, "done_check")[0]["turns_remaining"], 1);
    assert_eq!(
        only(&trace, "task_end")["done_check"],
        json!({"runs": 2, "reopens": 1, "limit": 5, "met": false, "stopped_by": "max_turns"})
    );
    assert_eq!(run_a.result(), "b");

    // (b) Inside the reopened attempt.
    let server = common::ScriptedServer::start(vec![
        end_turn("a"),
        tool_turn("toolu_1", "no-such-tool", json!({})),
    ]);
    let inside = project(&http_manifest(
        &server.endpoint,
        &done_check(ALWAYS_FAILS, 5, ""),
        "  max_turns: 2\n",
    ));
    let run_b = run(&home, &inside, &[]);
    assert_eq!(run_b.output.status.code(), Some(0), "{}", run_b.context());
    let trace = run_b.trace();
    assert_runs(&trace, &[("failed", json!(3), true, 4)]);
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok", "{task_end}");
    assert_eq!(
        task_end["done_check"],
        json!({"runs": 1, "reopens": 1, "limit": 5, "met": false, "stopped_by": "max_turns"})
    );
    assert_eq!(run_b.result(), "a");
}

// ── The spend ceiling ─────────────────────────────────────────────────────────

const MAX_OUTPUT: u64 = 1_000;

/// A spend ceiling that refuses the reopened attempt's first call ends the task `ok` with the
/// answer that preceded the reopen.
#[test]
fn a_spend_ceiling_ends_a_reopened_attempt_ok() {
    if common::skip_without_host_support("a_spend_ceiling_ends_a_reopened_attempt_ok") {
        return;
    }
    let home = home_with_driver();
    let control_server = common::ScriptedServer::start(vec![end_turn("a")]);
    let control_project = project(&http_manifest(
        &control_server.endpoint,
        "",
        &format!("  max_tokens: {MAX_OUTPUT}\n"),
    ));
    let control = run_control(&home, &control_project);
    assert_eq!(
        control.output.status.code(),
        Some(0),
        "{}",
        control.context()
    );
    let control_trace = control.trace();
    let first_input = only(&control_trace, "inference")["input_tokens"]
        .as_u64()
        .unwrap();
    // Admits the first call (its input plus the output cap) and refuses the second, which carries
    // the first call's input again, plus its answer and the feedback.
    let ceiling = first_input + MAX_OUTPUT + first_input / 2;

    let server = common::ScriptedServer::start(vec![end_turn("a"), end_turn("b")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(ALWAYS_FAILS, 5, ""),
        &format!("  max_tokens: {MAX_OUTPUT}\n  max_session_tokens: {ceiling}\n"),
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 1, "{}", run.context());
    let trace = run.trace();
    assert_runs(&trace, &[("failed", json!(3), true, 4)]);
    assert_eq!(
        events(&trace, "spend_ceiling_reached").len(),
        1,
        "{trace:#?}"
    );
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(task_end["done_check"]["stopped_by"], "spend_ceiling");
    assert_eq!(run.result(), "a");
}

// ── The deadline ──────────────────────────────────────────────────────────────

const DEADLINE_SECS: u64 = 45;

/// A deadline that falls while the check runs kills it and ends the task with the answer as its
/// result; `task_end` and `session_end` both name the deadline.
#[test]
fn a_deadline_stops_a_running_check() {
    if common::skip_without_host_support("a_deadline_stops_a_running_check") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("answer")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check("bash -c 'echo $$ > check.pid; exec sleep 600'", 3, ""),
        "",
    ));
    let begun = Instant::now();
    let run = run(&home, &project, &["--deadline", &DEADLINE_SECS.to_string()]);
    assert!(
        begun.elapsed() < Duration::from_secs(90),
        "took {:?}",
        begun.elapsed()
    );

    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stderr.contains("error[E-RUN-053]"), "{}", run.context());
    assert!(
        run.stdout.contains("status:  deadline_reached"),
        "{}",
        run.context()
    );
    let trace = run.trace();
    let record = only(&trace, "done_check");
    assert_eq!(record["outcome"], "stopped", "{record}");
    assert_eq!(record["exit_code"], Value::Null);
    assert_eq!(record["reopened"], false);
    assert!(record["duration_ms"].as_u64().unwrap() < 60_000, "{record}");
    let checked = position(&trace, "done_check");
    let reached = position(&trace, "deadline_reached");
    let task_end = position(&trace, "task_end");
    let session_end = position(&trace, "session_end");
    // The deadline is recorded when it falls, while the check runs; the run's record follows
    // once the killed command is reaped.
    assert!(
        reached < checked && checked < task_end && task_end < session_end,
        "{trace:#?}"
    );
    assert_eq!(session_end, trace.len() - 1, "{trace:#?}");
    assert_eq!(trace[session_end]["exit_status"], "deadline_reached");
    assert_eq!(trace[task_end]["exit_status"], "deadline_reached");
    assert_eq!(trace[task_end]["done_check"]["stopped_by"], "deadline");
    assert_eq!(trace[task_end]["done_check"]["met"], false);
    assert_eq!(run.result(), "answer");

    let pid: i32 = fs::read_to_string(run.workdir().join("check.pid"))
        .expect("the check wrote its pid")
        .trim()
        .parse()
        .unwrap();
    assert!(
        !common::pid_alive(pid),
        "the check's process {pid} outlived the deadline"
    );
}

// ── The timeout ───────────────────────────────────────────────────────────────

/// A check that outruns `timeout_secs` is killed and counts as a failed run; the feedback says
/// what it printed, `Exit code: none`, and that the timeout stopped it.
#[test]
fn timeout_secs_kills_a_hung_check() {
    if common::skip_without_host_support("timeout_secs_kills_a_hung_check") {
        return;
    }
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("a"), end_turn("b")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(
            "bash -c 'echo started; sleep 30'",
            1,
            "    timeout_secs: 2\n",
        ),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let trace = run.trace();
    assert_runs(
        &trace,
        &[
            ("timed_out", Value::Null, true, 0),
            ("timed_out", Value::Null, false, 0),
        ],
    );
    for record in events(&trace, "done_check") {
        let duration = record["duration_ms"].as_u64().unwrap();
        assert!((2_000..=10_000).contains(&duration), "{record}");
    }
    assert_eq!(
        only(&trace, "task_end")["done_check"],
        json!({"runs": 2, "reopens": 1, "limit": 1, "met": false, "stopped_by": "limit"})
    );
    let feedback = last_user_text(&server.requests()[1]);
    assert!(feedback.contains("\nExit code: none\n"), "{feedback}");
    assert!(feedback.contains("Stdout:\nstarted\n"), "{feedback}");
    assert!(
        feedback.ends_with(
            "</untrusted-content>\nStopped after 2 s by lifecycle.done_check.timeout_secs."
        ),
        "{feedback}"
    );
}

// ── A person's cancel ─────────────────────────────────────────────────────────

/// A scratch `$HOME` for the in-process launch, set once for this whole binary: `set_var` is
/// process-global. Every `mur run` case passes its own `HOME`, so none reads this one.
fn scratch_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        path
    })
}

fn stage_agent(
    home: &TempDir,
    manifest_path: &Path,
    lifecycle: LifecycleConfig,
) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    let allowlisted_tools: HashSet<String> = runtime_manifest
        .artifacts
        .iter()
        .filter(|artifact| matches!(artifact.runtime, ArtifactRuntime::Tool))
        .map(|artifact| artifact.name.clone())
        .collect();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        Arc::new(local_registry),
        StageRequest {
            source_supply: None,
            expected_workdir_identity: None,
            session_deadline: None,
            manifest_file: None,
            credentials_file: None,
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_artifact: None,
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: Some(lifecycle),
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
        },
    )
    .unwrap()
}

fn rpc(addr: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let stream = TcpStream::connect(addr).expect("connects to the door");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = &stream;
    write!(
        writer,
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: \
         1.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    writer.flush().unwrap();
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut response = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        response.push_str(&line);
    }
    serde_json::from_str(&response).unwrap_or_else(|_| json!({"_raw": response}))
}

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn wait_for_trace(
    path: &Path,
    within: Duration,
    what: &str,
    done: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + within;
    loop {
        let trace = read_trace(path);
        if done(&trace) {
            return trace;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {trace:#?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A person's `CancelTask` while the check runs kills it and ends the task `canceled`, with the
/// cancel claimed for the done check; `GetTask` and `task_end` agree.
#[test]
fn a_cancel_during_the_check_cancels_the_task() {
    if common::skip_without_host_support("a_cancel_during_the_check_cancels_the_task") {
        return;
    }
    scratch_home();
    let server = common::ScriptedServer::start(vec![end_turn("an answer")]);
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let project_dir = TempDir::new().unwrap();
    let manifest_path = project_dir.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        format!(
            "name: done-check-cancel\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {0}\n      \
             api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {0}\n  shell:\n    \
             allow: [bash, sleep]\ninference:\n  transport: http\n  model: test-model\n  driver:\n    \
             artifact: {DRIVER}\n",
            server.endpoint
        ),
    )
    .unwrap();
    let staged = stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 8,
            done_check: Some(DoneCheck {
                command: "bash -c ': > dc-started; exec sleep 600'".to_string(),
                limit: 3,
                timeout_secs: 600,
            }),
            ..Default::default()
        },
    );
    let started_file = staged.accessible_workdir.join("dc-started");
    let trace_path = staged.records_dir.join("trace.jsonl");
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let url = url_rx
        .recv_timeout(common::CAPSULE_URL_WAIT)
        .expect("the capsule reports its URL");

    let sent = rpc(
        &url,
        "SendMessage",
        json!({
            "message": {"messageId": "m1", "role": "ROLE_USER",
                        "parts": [{"text": TASK, "mediaType": "text/plain"}]},
            "configuration": {"returnImmediately": true},
        }),
    );
    let task_id = sent["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task id in {sent}"))
        .to_string();
    let waited = Instant::now();
    while !started_file.exists() {
        assert!(
            waited.elapsed() < Duration::from_secs(120),
            "the check never started: {:#?}",
            read_trace(&trace_path)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let canceled = rpc(&url, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    let trace = wait_for_trace(&trace_path, Duration::from_secs(10), "task_end", |trace| {
        !events(trace, "task_end").is_empty()
    });
    assert_eq!(only(&trace, "done_check")["outcome"], "stopped");
    assert_eq!(only(&trace, "task_canceled")["phase"], "done_check");
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "canceled", "{trace:#?}");
    assert_eq!(task_end["done_check"]["stopped_by"], Value::Null);
    assert_eq!(task_end["done_check"]["met"], false);
    assert_eq!(
        rpc(&url, "GetTask", json!({"id": task_id}))["result"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );
}

// ── Containment ───────────────────────────────────────────────────────────────

/// The check runs under the capsule's own containment and environment: it can no more write
/// outside the work folder than the agent's shell can, and its `HOME` is the session's.
#[test]
fn the_check_runs_in_the_capsule_s_containment() {
    if common::skip_without_host_support("the_check_runs_in_the_capsule_s_containment") {
        return;
    }
    let outside = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("done-check-outside-{}", std::process::id()));
    fs::create_dir_all(&outside).unwrap();
    let from_tool = outside.join("from-tool");
    let from_check = outside.join("from-check");
    let _ = fs::remove_file(&from_tool);
    let _ = fs::remove_file(&from_check);

    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        tool_turn(
            "toolu_1",
            "bash",
            json!({"command": format!("echo x > {}", from_tool.display())}),
        ),
        end_turn("done"),
        end_turn("done"),
    ]);
    // The write comes last, so a refused write is the run's exit status.
    let check = format!(
        "bash -c 'echo HOME=$HOME; echo y > {}'",
        from_check.display()
    );
    let project = project(&http_manifest(
        &server.endpoint,
        &done_check(&check, 1, ""),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert!(
        !from_tool.exists(),
        "precondition: the agent's shell could write outside the work folder, so this host \
         contains nothing to compare against: {}",
        run.context()
    );
    assert!(
        !from_check.exists(),
        "the check wrote outside the work folder"
    );
    let trace = run.trace();
    assert_eq!(events(&trace, "done_check")[0]["outcome"], "failed");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    let feedback = last_user_text(&requests[2]);
    let session_home = run.records_dir().join("home");
    assert!(
        feedback.contains(&format!("HOME={}\n", session_home.display())),
        "the check's HOME is the session's {}: {feedback}",
        session_home.display()
    );
    let _ = fs::remove_dir_all(&outside);
}

// ── Validation ────────────────────────────────────────────────────────────────

/// A check whose program `capabilities.shell.allow` does not list is refused before any session
/// starts, naming the field.
#[test]
fn an_invalid_block_is_refused() {
    let home = TempDir::new().unwrap();
    let project = project(&http_manifest(
        "http://127.0.0.1:9",
        &done_check("cargo test --quiet", 1, ""),
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stderr.contains("error[E-MAN-003]"), "{}", run.context());
    assert!(
        run.stderr.contains("lifecycle.done_check.command")
            && run
                .stderr
                .contains("runs 'cargo', which is not in capabilities.shell.allow"),
        "{}",
        run.context()
    );
    assert!(!run.stdout.contains("records: "), "{}", run.context());
    assert!(
        !common::records_root(home.path()).exists(),
        "no session was started: {}",
        run.context()
    );
}
