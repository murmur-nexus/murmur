//! What a launch reports for a task that did not complete, on a queue capsule that exits after its
//! task.
//!
//! The `mur run` cases drive the real binary against the real anthropic driver fixture and a
//! scripted provider, because the properties are the process's exit code, its `status:` line and
//! its stderr. The in-process cases launch the same capsule through `launch_session`, where the
//! launch's own result and the A2A door are what is measured.
//!
//! Every `--task` case sets `MURMUR_A2A_TIMEOUT_SECS=1` and reads only the records of the task it
//! gave, matched on the `task_start` that names it, so a run the launch makes after that task
//! cannot satisfy or break an assertion.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, RuntimeError, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Say hello.";

/// A chat-completions body: well-formed JSON in another provider's shape. The anthropic driver
/// refuses it because it has no `stop_reason`, so the task fails with `driver_error`.
const CHAT_COMPLETIONS_BODY: &str = r#"{"id":"x","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;

/// A body cut off in transit, mid-JSON.
const TRUNCATED_BODY: &str = r#"{"content":[{"type":"text","text":"hel"#;

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A turn that asks for a tool the capsule does not have, so the loop wants a second turn.
fn tool_call(id: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": "toolu_outcome", "name": "no-such-tool", "input": {}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the driver published into it.
fn home_with_driver() -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &artifact).success();
    home
}

/// A queue capsule that exits after its task, reaching `endpoint` through the driver's gateway.
/// `inference_extra` is spliced into the `inference:` block.
fn queue_manifest(name: &str, endpoint: &str, inference_extra: &str) -> String {
    format!(
        "name: {name}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         lifecycle:\n  task_acceptance: queue\n  after_task: exit\n\
         inference:\n  transport: http\n  model: test-model\n{inference_extra}  driver:\n    \
         artifact: {DRIVER}\n"
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

/// `mur run --task … --lifecycle-after-task exit` on `project`.
fn run_queue_exit(home: &TempDir, project: &Project) -> Run {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MURMUR_A2A_TIMEOUT_SECS", "1")
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.dir.path())
        .args([
            "run",
            "--manifest",
            project.manifest.to_str().unwrap(),
            "--task",
            TASK,
            "--lifecycle-after-task",
            "exit",
            "--verbose",
        ])
        .output()
        .unwrap();
    Run::of(output)
}

/// What one `mur run` left behind.
struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }

    /// Every `status:` line `mur run` printed.
    fn status_lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with("status:"))
            .collect()
    }

    /// Every stderr line naming `code`.
    fn error_lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(&format!("error[{code}]")))
            .collect()
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.workdir().join("trace.jsonl"))
    }
}

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn events_named<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// The records of the `task.md` task this launch was given: its `task_start` is the first, and
/// every record returned names its id.
struct TaskRecords<'a> {
    task_id: String,
    trace: &'a [Value],
}

impl<'a> TaskRecords<'a> {
    fn of_task_md(trace: &'a [Value]) -> Self {
        let start = events_named(trace, "task_start")
            .into_iter()
            .find(|event| event["source"] == "task_md")
            .unwrap_or_else(|| panic!("no task_md task_start in {trace:?}"));
        Self {
            task_id: start["task_id"].as_str().unwrap().to_string(),
            trace,
        }
    }

    fn of_task(trace: &'a [Value], task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            trace,
        }
    }

    fn named(&self, event_type: &str) -> Vec<&'a Value> {
        events_named(self.trace, event_type)
            .into_iter()
            .filter(|event| event["task_id"] == self.task_id.as_str())
            .collect()
    }

    fn position(&self, event_type: &str) -> usize {
        self.trace
            .iter()
            .position(|event| {
                event["event_type"] == event_type && event["task_id"] == self.task_id.as_str()
            })
            .unwrap_or_else(|| panic!("no {event_type} for {}", self.task_id))
    }

    fn task_end_status(&self) -> String {
        let ends = self.named("task_end");
        assert_eq!(ends.len(), 1, "one task_end for {}", self.task_id);
        ends[0]["exit_status"].as_str().unwrap().to_string()
    }

    /// The task's one `task_failed` line, asserted to carry `cause`, a turn and a reason, and to
    /// come before its `task_end`.
    fn sole_failure(&self, cause: &str) -> &'a Value {
        let failed = self.named("task_failed");
        assert_eq!(failed.len(), 1, "one task_failed for {}", self.task_id);
        let failed = failed[0];
        assert_eq!(failed["cause"], cause, "{failed}");
        assert!(
            !failed["reason"].as_str().unwrap().is_empty(),
            "a reason: {failed}"
        );
        assert!(
            self.position("task_failed") < self.position("task_end"),
            "task_failed precedes task_end"
        );
        failed
    }
}

fn session_end_status(trace: &[Value]) -> String {
    let ends = events_named(trace, "session_end");
    assert_eq!(ends.len(), 1, "one session_end");
    ends[0]["exit_status"].as_str().unwrap().to_string()
}

/// `run` ended its task `failed` with `cause`: exit 1, `status:  failed`, one `E-RUN-040` line
/// carrying the `task_failed` reason, and `failed` on both terminal trace records.
fn assert_failed_with<'t>(run: &Run, trace: &'t [Value], cause: &str) -> &'t Value {
    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());
    assert_eq!(run.status_lines(), ["status:  failed"], "{}", run.context());

    let task = TaskRecords::of_task_md(trace);
    let failed = task.sole_failure(cause);
    assert!(failed["turn"].is_u64(), "a turn: {failed}");
    let reason = failed["reason"].as_str().unwrap();

    let error_lines = run.error_lines("E-RUN-040");
    assert_eq!(error_lines.len(), 1, "{}", run.context());
    assert!(
        error_lines[0].starts_with("error[E-RUN-040]: the task ended failed: "),
        "{}",
        error_lines[0]
    );
    assert!(
        run.stderr.contains(&format!(
            "error[E-RUN-040]: the task ended failed: {reason}"
        )),
        "the E-RUN-040 reason is the task_failed reason {reason:?}; {}",
        run.context()
    );

    assert_eq!(task.task_end_status(), "failed");
    assert_eq!(session_end_status(trace), "failed");
    failed
}

// ── A task that completes ─────────────────────────────────────────────────────

#[test]
fn s1_a_completed_task_is_ok_and_exits_zero() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("msg_1", "hello")]);
    let project = project(&queue_manifest("outcome-ok", &server.endpoint, ""));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(run.status_lines(), ["status:  ok"], "{}", run.context());
    assert!(run.error_lines("E-RUN-040").is_empty(), "{}", run.context());

    let task = TaskRecords::of_task_md(&trace);
    assert_eq!(task.task_end_status(), "ok");
    assert!(task.named("task_failed").is_empty());
    assert_eq!(session_end_status(&trace), "ok");
}

// ── A driver call that failed ─────────────────────────────────────────────────

#[test]
fn s2_a_body_in_another_providers_shape_fails_the_task() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![CHAT_COMPLETIONS_BODY.to_string()]);
    let project = project(&queue_manifest("outcome-chat", &server.endpoint, ""));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    let failed = assert_failed_with(&run, &trace, "driver_error");
    println!("task_failed: {failed}");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn s3_a_body_cut_off_in_transit_fails_the_task() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![TRUNCATED_BODY.to_string()]);
    let project = project(&queue_manifest("outcome-truncated", &server.endpoint, ""));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    let failed = assert_failed_with(&run, &trace, "driver_error");
    println!("task_failed: {failed}");
}

// ── A response the loop cannot act on ─────────────────────────────────────────

/// The `truncation-driver` fixture in `NOSTOP` mode answers with a `stop_reason` the loop has no
/// branch for, without making an HTTP call.
#[test]
fn s7_an_unsupported_stop_reason_fails_the_task() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact(
        artifacts.path(),
        "truncation-driver",
        "0.1.0",
        &common::fixture_path("truncation-driver/tool/truncation-driver.wasm"),
    );
    common::publish_local(&home, &artifact).success();
    let endpoint = "http://127.0.0.1:9";
    let project = project(&format!(
        "name: outcome-nostop\nversion: 0.1.0\nartifacts:\n  - name: truncation-driver\n    \
         version: 0.1.0\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         lifecycle:\n  task_acceptance: queue\n  after_task: exit\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: \
         truncation-driver\n    config:\n      mode: NOSTOP\n"
    ));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    let failed = assert_failed_with(&run, &trace, "malformed_response");
    assert!(
        failed["reason"]
            .as_str()
            .unwrap()
            .contains("unsupported stop_reason"),
        "{failed}"
    );
}

// ── A spent turn budget ───────────────────────────────────────────────────────

#[test]
fn s8_a_spent_turn_budget_is_max_turns_reached_and_exits_non_zero() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![tool_call("msg_1")]);
    let project = project(&queue_manifest(
        "outcome-max-turns",
        &server.endpoint,
        "  max_turns: 1\n",
    ));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());
    assert_eq!(
        run.status_lines(),
        ["status:  max_turns_reached"],
        "{}",
        run.context()
    );
    let error_lines = run.error_lines("E-RUN-040");
    assert_eq!(error_lines.len(), 1, "{}", run.context());
    assert!(
        error_lines[0].contains("the task ended max_turns_reached: ")
            && error_lines[0].contains("inference.max_turns"),
        "{}",
        error_lines[0]
    );

    let task = TaskRecords::of_task_md(&trace);
    assert_eq!(task.task_end_status(), "max_turns_reached");
    assert!(task.named("task_failed").is_empty(), "{trace:?}");
    assert_eq!(session_end_status(&trace), "max_turns_reached");
}

// ── The launch outcome is never replaced ──────────────────────────────────────

/// The provider would answer a second call cleanly. Whatever the launch runs after the failed
/// task, the launch still reports the failure.
#[test]
fn s13_a_failed_task_is_not_replaced_by_a_later_clean_run() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![
        TRUNCATED_BODY.to_string(),
        end_turn("msg_2", "a clean run after the failure"),
    ]);
    let project = project(&queue_manifest("outcome-combine", &server.endpoint, ""));
    let run = run_queue_exit(&home, &project);
    let trace = run.trace();

    assert_failed_with(&run, &trace, "driver_error");
}

// ── In-process: the launch result and the A2A door ────────────────────────────

/// A scratch `$HOME` for this whole test binary, so an in-process launch resolves its
/// conversation record there rather than in the developer's home. Set once: `set_var` is
/// process-global.
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

/// A capsule reaching `endpoint` through the driver, staged in-process under `lifecycle`.
fn stage(
    endpoint: &str,
    name: &str,
    inference_extra: &str,
    lifecycle: LifecycleConfig,
) -> (TempDir, capsule_runtime::StagedSession) {
    scratch_home();
    let home = home_with_driver();
    let project = tempfile::tempdir().unwrap().keep();
    let manifest_path = project.join("murmur.yaml");
    fs::write(
        &manifest_path,
        format!(
            "name: {name}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
             {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
             api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
             inference:\n  transport: http\n  model: test-model\n{inference_extra}  driver:\n    \
             artifact: {DRIVER}\n"
        ),
    )
    .unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let requested_artifacts = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        })
        .collect();
    let allowlisted_tools: HashSet<String> = runtime_manifest
        .artifacts
        .iter()
        .filter(|artifact| matches!(artifact.runtime, ArtifactRuntime::Tool))
        .map(|artifact| artifact.name.clone())
        .collect();

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
            manifest_dir: project,
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
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
            ignore_task_file: false,
        },
    )
    .unwrap();
    (home, staged)
}

fn queue_lifecycle(after_task: AfterTask) -> LifecycleConfig {
    LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task,
        queue_depth: 8,
        ..Default::default()
    }
}

/// One capsule launched on its own thread: its door, its trace, and what `launch_session`
/// returned once it did.
struct Capsule {
    url: String,
    trace_path: PathBuf,
    launched: mpsc::Receiver<Result<(), RuntimeError>>,
}

impl Capsule {
    fn launch(staged: capsule_runtime::StagedSession) -> Self {
        let trace_path = staged.workdir.join("trace.jsonl");
        let (url_tx, url_rx) = mpsc::channel::<String>();
        let (result_tx, launched) = mpsc::channel();
        std::thread::spawn(move || {
            let result = launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            });
            let _ = result_tx.send(result.map(|_| ()));
        });
        let url = url_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("timed out waiting for the capsule URL");
        Self {
            url,
            trace_path,
            launched,
        }
    }

    fn launch_result(&self) -> Result<(), RuntimeError> {
        self.launched
            .recv_timeout(Duration::from_secs(90))
            .expect("the launch returned")
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.trace_path)
    }
}

fn http_post_json(addr: &str, body: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = &stream;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    writer.write_all(request.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut response_body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        response_body.push_str(&line);
    }
    serde_json::from_str(&response_body).unwrap_or_else(|_| json!({"_raw": response_body}))
}

fn rpc(addr: &str, method: &str, params: Value) -> Value {
    http_post_json(
        addr,
        &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
    )
}

fn send_message(addr: &str, text: &str) -> String {
    let sent = rpc(
        addr,
        "message/send",
        json!({"message": {"messageId": "msg-1", "role": "user", "parts": [{"text": text}]}}),
    );
    sent["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("message/send returned no task id: {sent}"))
        .to_string()
}

fn poll_until_state(addr: &str, task_id: &str, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let response = rpc(addr, "tasks/get", json!({"id": task_id}));
        if response["result"]["status"]["state"].as_str() == Some(expected) {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for state '{expected}'; last response: {response}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send `text` over `message/stream` and read the connection to its end. Returns the task id and
/// the last status event the stream carried.
fn stream_to_end(addr: &str, text: &str) -> (String, Value) {
    let stream = TcpStream::connect(addr).expect("should connect for SSE");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {"message": {"messageId": "msg-1", "role": "user", "parts": [{"text": text}]}}
    })
    .to_string();
    {
        let mut writer = &stream;
        writer
            .write_all(
                format!(
                    "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let _ = writer.flush();
    }
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }

    let mut last_status: Option<Value> = None;
    let mut current_type = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(error) => panic!("the SSE connection errored instead of closing: {error}"),
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        if let Some(rest) = line.strip_prefix("event: ") {
            current_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            if current_type == "status" {
                last_status = serde_json::from_str(rest).ok();
            }
            if last_status
                .as_ref()
                .is_some_and(|status| status["final"] == json!(true))
            {
                break;
            }
        }
    }
    let last_status = last_status.expect("the stream carried at least one status event");
    let task_id = last_status["id"].as_str().unwrap().to_string();
    (task_id, last_status)
}

fn assert_did_not_complete(result: Result<(), RuntimeError>, expected: &str) -> String {
    match result {
        Err(RuntimeError::TaskDidNotComplete {
            exit_status,
            reason,
        }) => {
            assert_eq!(exit_status, expected, "reason: {reason}");
            assert!(!reason.is_empty());
            reason
        }
        other => panic!("expected TaskDidNotComplete({expected}), got {other:?}"),
    }
}

// ── A failed driver call on an A2A task ───────────────────────────────────────

#[test]
fn s4_a_failed_a2a_task_fails_the_launch_and_its_stream() {
    let server = common::ScriptedServer::start(vec![TRUNCATED_BODY.to_string()]);
    let (_home, staged) = stage(
        &server.endpoint,
        "outcome-a2a-exit",
        "",
        queue_lifecycle(AfterTask::Exit),
    );
    let capsule = Capsule::launch(staged);

    let (task_id, last_status) = stream_to_end(&capsule.url, TASK);
    assert_eq!(last_status["status"]["state"], "failed", "{last_status}");
    assert_eq!(last_status["final"], json!(true), "{last_status}");

    let reason = assert_did_not_complete(capsule.launch_result(), "failed");
    let trace = capsule.trace();
    let task = TaskRecords::of_task(&trace, &task_id);
    let failed = task.sole_failure("driver_error");
    assert_eq!(failed["reason"].as_str().unwrap(), reason);
    assert_eq!(task.task_end_status(), "failed");
    assert_eq!(session_end_status(&trace), "failed");
}

#[test]
fn s5_a_failed_a2a_task_reads_failed_on_tasks_get() {
    let server = common::ScriptedServer::start(vec![TRUNCATED_BODY.to_string()]);
    let (_home, staged) = stage(
        &server.endpoint,
        "outcome-a2a-sleep",
        "",
        queue_lifecycle(AfterTask::Sleep),
    );
    let capsule = Capsule::launch(staged);

    let task_id = send_message(&capsule.url, TASK);
    poll_until_state(&capsule.url, &task_id, "failed");
    let trace = capsule.trace();
    let task = TaskRecords::of_task(&trace, &task_id);
    task.sole_failure("driver_error");
    assert_eq!(task.task_end_status(), "failed");
}

/// On the door: a spent turn budget leaves the task `failed`, the state its stream already
/// closed with.
#[test]
fn s8_a_spent_turn_budget_reads_failed_on_tasks_get() {
    let server = common::ScriptedServer::start(vec![tool_call("msg_1")]);
    let (_home, staged) = stage(
        &server.endpoint,
        "outcome-a2a-max-turns",
        "  max_turns: 1\n",
        queue_lifecycle(AfterTask::Sleep),
    );
    let capsule = Capsule::launch(staged);

    let task_id = send_message(&capsule.url, TASK);
    poll_until_state(&capsule.url, &task_id, "failed");
    let trace = capsule.trace();
    let task = TaskRecords::of_task(&trace, &task_id);
    assert_eq!(task.task_end_status(), "max_turns_reached");
    assert!(task.named("task_failed").is_empty());
}

// ── A cancelled task ──────────────────────────────────────────────────────────

#[test]
fn s10_a_cancelled_task_ends_the_launch_canceled() {
    let server = common::ScriptedServer::start_with_delay(
        vec![end_turn("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let (_home, staged) = stage(
        &server.endpoint,
        "outcome-cancel",
        "",
        queue_lifecycle(AfterTask::Exit),
    );
    let capsule = Capsule::launch(staged);

    let task_id = send_message(&capsule.url, TASK);
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.requests().is_empty() {
        assert!(Instant::now() < deadline, "the provider was never called");
        std::thread::sleep(Duration::from_millis(50));
    }
    let canceled = rpc(&capsule.url, "tasks/cancel", json!({"id": task_id}));
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );

    let reason = assert_did_not_complete(capsule.launch_result(), "canceled");
    assert_eq!(reason, "the task was canceled");
    let trace = capsule.trace();
    let task = TaskRecords::of_task(&trace, &task_id);
    assert_eq!(task.named("task_canceled").len(), 1);
    assert_eq!(task.task_end_status(), "canceled");
    assert!(task.named("task_failed").is_empty());
    assert_eq!(session_end_status(&trace), "canceled");
}

// ── `mur trace show` and `mur trace steps` render the failure ─────────────────

#[test]
fn trace_show_and_steps_render_task_failed() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![TRUNCATED_BODY.to_string()]);
    let project = project(&queue_manifest("outcome-render", &server.endpoint, ""));
    let run = run_queue_exit(&home, &project);
    let trace_path = run.workdir().join("trace.jsonl");
    let trace = run.trace();
    let reason = TaskRecords::of_task_md(&trace).sole_failure("driver_error")["reason"]
        .as_str()
        .unwrap()
        .to_string();

    let show = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "show", trace_path.to_str().unwrap()])
        .assert()
        .success();
    let show = String::from_utf8(show.get_output().stdout.clone()).unwrap();
    println!("{show}");
    assert!(show.contains("task_failed  driver_error"), "{show}");
    let first_line = reason.lines().next().unwrap();
    assert!(show.contains(first_line), "{show}");

    let steps = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "steps", trace_path.to_str().unwrap()])
        .assert()
        .success();
    let steps = String::from_utf8(steps.get_output().stdout.clone()).unwrap();
    println!("{steps}");
    assert!(steps.contains("task_failed"), "{steps}");
    assert!(steps.contains("driver_error"), "{steps}");
}
