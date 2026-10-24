//! `lifecycle.keep_working`: a task kept working past its turn ends until a budget is spent.
//!
//! Driven through the real `mur` binary against the anthropic driver fixture and a scripted
//! provider, and through the fixture process driver and the fake harness, because the properties
//! are what the provider was sent and what the trace and `out/result.txt` say afterwards. The
//! cancel case launches in-process, as `cancel.rs` does, so it can reach the capsule's door.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, KeepWorking, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const PROCESS_DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Write the report.";
const MESSAGE: &str = "Keep working on the task. Check your work against what was asked.";

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

fn tool_turn(id: &str, name: &str) -> String {
    response(
        json!([{"type": "tool_use", "id": id, "name": name, "input": {}}]),
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

/// `lifecycle.keep_working` with `MESSAGE` and `limit`.
fn keep_working(limit: u32) -> String {
    format!("  keep_working:\n    message: \"{MESSAGE}\"\n    limit: {limit}\n")
}

/// An http capsule on `endpoint`; `lifecycle` and `inference` are spliced into their blocks.
fn http_manifest(endpoint: &str, lifecycle: &str, inference: &str) -> String {
    format!(
        "name: keep-working\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         lifecycle:\n  task_acceptance: single\n{lifecycle}inference:\n  transport: http\n  \
         model: test-model\n{inference}  driver:\n    artifact: {DRIVER}\n"
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

/// The last message of a provider request.
fn last_message(request: &Value) -> &Value {
    request["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .unwrap_or_else(|| panic!("no messages in {request}"))
}

/// Whether `request` carries `MESSAGE` anywhere in its messages.
fn carries_message(request: &Value) -> bool {
    request["messages"].to_string().contains(MESSAGE)
}

// ── S1: off ───────────────────────────────────────────────────────────────────

/// With no `lifecycle.keep_working` the task ends at its first turn end, exactly as before.
#[test]
fn without_the_block_the_task_ends_at_its_first_turn_end() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("first")]);
    let project = project(&http_manifest(&server.endpoint, "", ""));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 1);
    let trace = run.trace();
    assert_eq!(events(&trace, "inference").len(), 1, "{trace:#?}");
    assert!(events(&trace, "keep_working").is_empty(), "{trace:#?}");
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert!(task_end.get("keep_working").is_none(), "{task_end}");
    assert_eq!(run.result(), "first");
}

// ── S2 + S3: the limit, and the message is the operator's ─────────────────────

/// `limit: 2`: two continuations, each recorded after the turn end it answers and before the next
/// call, the task ending `ok` with the third turn end's answer; the provider is sent the
/// operator's message verbatim, unfenced, and nothing else changes in what it is sent.
#[test]
fn a_task_is_kept_working_until_the_limit_with_the_operator_s_message() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        end_turn("one"),
        tool_turn("toolu_1", "session-status"),
        end_turn("two"),
        end_turn("three"),
    ]);
    let kept = project(&http_manifest(&server.endpoint, &keep_working(2), ""));
    let run = run(&home, &kept, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 4, "{requests:#?}");
    let trace = run.trace();
    let records = events(&trace, "keep_working");
    assert_eq!(records.len(), 2, "{trace:#?}");
    for (record, number) in records.iter().zip([1, 2]) {
        assert_eq!(record["number"], number);
        assert_eq!(record["limit"], 2);
        assert_eq!(record["message_bytes"], MESSAGE.len());
        assert_eq!(record["turns_remaining"], Value::Null);
    }
    // Each continuation follows the `inference` of the turn end it answers and precedes the next.
    let inferences = positions(&trace, "inference");
    let continued = positions(&trace, "keep_working");
    assert_eq!(inferences.len(), 4, "{trace:#?}");
    assert!(inferences[0] < continued[0] && continued[0] < inferences[1]);
    assert!(inferences[2] < continued[1] && continued[1] < inferences[3]);

    assert_eq!(events(&trace, "task_start").len(), 1);
    assert!(events(&trace, "task_reopened").is_empty());
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(task_end["turns"], 4);
    assert_eq!(
        task_end["keep_working"],
        json!({"used": 2, "limit": 2, "stopped_by": "limit"})
    );
    assert_eq!(
        only(&trace, "session_status")["status"]["keep_working"],
        json!({"used": 1, "left": 1})
    );
    assert_eq!(run.result(), "three");

    // S3: after each turn end the last message is a user message whose one text block is exactly
    // the operator's text; it is absent from the first request. The driver marks the last block
    // for prompt caching, which is the driver's own key, not text.
    assert!(!carries_message(&requests[0]), "{}", requests[0]);
    for index in [1, 3] {
        let last = last_message(&requests[index]);
        assert_eq!(last["role"], "user", "{last}");
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1, "request {index}: {last}");
        assert_eq!(blocks[0]["type"], "text", "request {index}: {last}");
        assert_eq!(blocks[0]["text"], MESSAGE, "request {index}: {last}");
    }

    // The first request's system prompt is the same with and without the block.
    let control_server = common::ScriptedServer::start(vec![end_turn("first")]);
    let control_project = project(&http_manifest(&control_server.endpoint, "", ""));
    let control = run_control(&home, &control_project);
    assert_eq!(
        control.output.status.code(),
        Some(0),
        "{}",
        control.context()
    );
    // The two capsules differ only in the provider's port, which the allowlist line names.
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

/// [`run`] under its own name, so a second launch in one case reads as the control it is.
fn run_control(home: &TempDir, project: &Project) -> Run {
    run(home, project, &[])
}

// ── S4 + S5: the turn limit ───────────────────────────────────────────────────

/// `max_turns` spent inside a continued attempt ends the task `ok` with the last turn end's
/// answer, not the turn-limit marker.
#[test]
fn the_turn_limit_inside_a_continuation_ends_the_task_with_the_last_answer() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        end_turn("one"),
        tool_turn("toolu_1", "no-such-tool"),
        tool_turn("toolu_2", "no-such-tool"),
    ]);
    let project = project(&http_manifest(
        &server.endpoint,
        &keep_working(10),
        "  max_turns: 3\n",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let trace = run.trace();
    assert_eq!(events(&trace, "inference").len(), 3, "{trace:#?}");
    assert_eq!(events(&trace, "keep_working").len(), 1, "{trace:#?}");
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(
        task_end["keep_working"],
        json!({"used": 1, "limit": 10, "stopped_by": "max_turns"})
    );
    assert_eq!(only(&trace, "session_end")["exit_status"], "ok");
    assert_eq!(run.result(), "one");
}

/// `max_turns` spent at a turn end stops the task there, with that turn end's answer.
#[test]
fn the_turn_limit_at_a_turn_end_stops_the_task_with_that_answer() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("one"), end_turn("two")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &keep_working(10),
        "  max_turns: 2\n",
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 2);
    let trace = run.trace();
    let record = only(&trace, "keep_working");
    assert_eq!(record["turns_remaining"], 1);
    assert_eq!(
        only(&trace, "task_end")["keep_working"],
        json!({"used": 1, "limit": 10, "stopped_by": "max_turns"})
    );
    assert_eq!(run.result(), "two");
}

// ── S6: the spend ceiling ─────────────────────────────────────────────────────

const MAX_OUTPUT: u64 = 1_000;

/// A spend ceiling that refuses the continued attempt's first call ends the task `ok` with the
/// answer it holds, and the refused call is not retried.
#[test]
fn a_spend_ceiling_ends_a_kept_working_task_with_the_last_answer() {
    let home = home_with_driver();
    let control_server = common::ScriptedServer::start(vec![end_turn("one")]);
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
    // the first call's input again, plus its answer and the message.
    let ceiling = first_input + MAX_OUTPUT + first_input / 2;

    let server = common::ScriptedServer::start(vec![end_turn("one"), end_turn("two")]);
    let project = project(&http_manifest(
        &server.endpoint,
        &keep_working(10),
        &format!("  max_tokens: {MAX_OUTPUT}\n  max_session_tokens: {ceiling}\n"),
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    assert_eq!(server.requests().len(), 1, "{}", run.context());
    let trace = run.trace();
    assert_eq!(events(&trace, "keep_working").len(), 1, "{trace:#?}");
    assert_eq!(
        events(&trace, "spend_ceiling_reached").len(),
        1,
        "{trace:#?}"
    );
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "ok");
    assert_eq!(
        task_end["keep_working"],
        json!({"used": 1, "limit": 10, "stopped_by": "spend_ceiling"})
    );
    assert_eq!(run.result(), "one");
}

// ── S7: the deadline ──────────────────────────────────────────────────────────

const DEADLINE_SECS: u64 = 45;

/// A deadline that falls while the task is kept working ends the task `ok` with the last turn
/// end's answer; the session still ends `deadline_reached`.
#[test]
fn the_deadline_ends_a_kept_working_task_ok_and_the_session_deadline_reached() {
    let home = home_with_driver();
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&asked);
    let server = common::ScriptedServer::start_answering(200, move |_| {
        let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::sleep(Duration::from_secs(1));
        end_turn(&format!("answer {n}"))
    });
    let project = project(&http_manifest(&server.endpoint, &keep_working(1000), ""));
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
    let records = events(&trace, "keep_working");
    assert!(!records.is_empty(), "{trace:#?}");
    let last_kept = *positions(&trace, "keep_working").last().unwrap();
    let reached = position(&trace, "deadline_reached");
    let task_end = position(&trace, "task_end");
    let session_end = position(&trace, "session_end");
    assert!(
        last_kept < reached && reached < task_end && task_end < session_end,
        "{trace:#?}"
    );
    assert_eq!(session_end, trace.len() - 1, "{trace:#?}");
    assert_eq!(trace[session_end]["exit_status"], "deadline_reached");
    assert_eq!(trace[task_end]["exit_status"], "ok");
    assert_eq!(
        trace[task_end]["keep_working"],
        json!({"used": records.len(), "limit": 1000, "stopped_by": "deadline"})
    );
    let answered = events(&trace, "inference")
        .iter()
        .filter(|inference| inference["decision"] == "end_turn")
        .count();
    assert_eq!(run.result(), format!("answer {answered}"));
}

// ── S8: a person's cancel ─────────────────────────────────────────────────────

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

fn wait_for_trace(path: &Path, what: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(120);
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

/// A person's `CancelTask` ends a kept-working task `canceled`: no stop is recorded, and `GetTask`
/// and `task_end` agree.
#[test]
fn a_person_s_cancel_ends_a_kept_working_task_canceled() {
    scratch_home();
    let server = common::ScriptedServer::start_answering(50, move |_| {
        std::thread::sleep(Duration::from_secs(3));
        end_turn("an answer")
    });
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
            "name: keep-working-cancel\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {0}\n      \
             api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {0}\n\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n",
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
            keep_working: Some(KeepWorking {
                message: MESSAGE.to_string(),
                limit: 1000,
            }),
            ..Default::default()
        },
    );
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
    wait_for_trace(&trace_path, "the first keep_working", |trace| {
        !events(trace, "keep_working").is_empty()
    });
    let canceled = rpc(&url, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    let trace = wait_for_trace(&trace_path, "task_end", |trace| {
        !events(trace, "task_end").is_empty()
    });
    let task_end = only(&trace, "task_end");
    assert_eq!(task_end["exit_status"], "canceled", "{trace:#?}");
    assert_eq!(task_end["keep_working"]["stopped_by"], Value::Null);
    assert_eq!(
        task_end["keep_working"]["used"],
        events(&trace, "keep_working").len()
    );
    assert_eq!(
        rpc(&url, "GetTask", json!({"id": task_id}))["result"]["status"]["state"],
        "TASK_STATE_CANCELED"
    );
}

// ── S9: process transport ─────────────────────────────────────────────────────

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Under `transport: process` the message is the resumed harness session's prompt, so the
/// harness answers it from the conversation the task began.
#[test]
fn a_process_task_is_kept_working_in_its_harness_session() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        PROCESS_DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    common::publish_local(&home, &driver).success();

    // Only this capsule's `PATH` points here, so `fixture-cli` is the fake harness, run under the
    // `memory` profile: it keeps every prompt its session was given and answers with all of them.
    let bin_dir = project.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let script = project.path().join("fake-harness");
    fs::copy(common::fixture_path("process-driver/fake-harness"), &script).unwrap();
    make_executable(&script);
    let wrapper = bin_dir.join("fixture-cli");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/bash\nexport FIXTURE_HARNESS_PROFILE=memory\nexec {} \"$@\"\n",
            script.display()
        ),
    )
    .unwrap();
    make_executable(&wrapper);
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: keep-working-process\nversion: 0.1.0\nartifacts:\n  - name: {PROCESS_DRIVER}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n\
             lifecycle:\n  task_acceptance: single\n  after_task: exit\n  conversation: threaded\n  \
             keep_working:\n    message: \"what is the secret\"\n    limit: 1\n\
             capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
             inference:\n  transport: process\n  driver:\n    artifact: {PROCESS_DRIVER}\n"
        ),
    )
    .unwrap();

    let output = Command::cargo_bin("mur")
        .unwrap()
        .env_clear()
        .env("HOME", home.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
            ),
        )
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--verbose",
            "--context",
            "ctx_keep",
            "--task",
            "the secret is PLUM-42",
        ])
        .timeout(Duration::from_secs(300))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    let records = common::parse_records_from_stdout(&text);
    let trace = common::read_whole_trace(&records.join("trace.jsonl"));

    assert_eq!(events(&trace, "keep_working").len(), 1, "{trace:#?}");
    let launches = events(&trace, "harness_start");
    assert_eq!(launches.len(), 2, "{launches:#?}");
    assert_eq!(launches[0]["session_mode"], "new");
    assert_eq!(launches[1]["session_mode"], "resume");
    assert_eq!(
        launches[0]["harness_session_id"], launches[1]["harness_session_id"],
        "the continuation resumes the session the task began"
    );
    let result = fs::read_to_string(records.join("out").join("result.txt")).unwrap();
    assert!(
        result.contains("PLUM-42") && result.contains("what is the secret"),
        "the resumed session was handed the message and kept the task: {result}"
    );
    assert_eq!(
        only(&trace, "task_end")["keep_working"],
        json!({"used": 1, "limit": 1, "stopped_by": "limit"})
    );
}

// ── S10: validation ───────────────────────────────────────────────────────────

/// `limit: 0` is refused before any session starts, naming the field.
#[test]
fn a_zero_limit_is_refused_before_the_session_starts() {
    let home = TempDir::new().unwrap();
    let project = project(&http_manifest(
        "http://127.0.0.1:9",
        "  keep_working:\n    message: go on\n    limit: 0\n",
        "",
    ));
    let run = run(&home, &project, &[]);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stderr.contains("error[E-MAN-003]"), "{}", run.context());
    assert!(
        run.stderr.contains("lifecycle.keep_working.limit"),
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
