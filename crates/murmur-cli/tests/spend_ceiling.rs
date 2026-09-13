//! Spend ceilings: a driver call that would cross `inference.max_session_tokens` or
//! `spend.machine_tokens_per_day` is refused before it reaches the provider.
//!
//! Driven against a scripted provider the real anthropic driver reaches through the inference
//! gateway, so every "nothing was sent" assertion is a count of requests that crossed a socket.
//! Ceilings are derived from a control run's own trace rather than hard-coded, because the
//! runtime's token counts move with the system prompt and the tool inventory.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
/// `inference.max_tokens` in every manifest here, so each agent turn reserves exactly this much
/// output.
const MAX_OUTPUT: u64 = 1_000;
const W_SEC_026_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-026";

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

/// A turn that asks for a tool the capsule does not have, so the loop takes a second turn.
fn tool_call(id: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": "toolu_spend", "name": "no-such-tool", "input": {}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn two_turns() -> Vec<String> {
    vec![tool_call("msg_1"), end_turn("msg_2", "done")]
}

// ── Homes, projects and runs ──────────────────────────────────────────────────

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

/// Put `spend.machine_tokens_per_day: ceiling` in `home`'s global config.
fn set_machine_ceiling(home: &TempDir, ceiling: u64) {
    let path = home.path().join(".murmur").join("config.yaml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut config = fs::read_to_string(&path).unwrap_or_default();
    config.push_str(&format!("spend:\n  machine_tokens_per_day: {ceiling}\n"));
    fs::write(&path, config).unwrap();
}

fn http_manifest(name: &str, endpoint: &str, inference_extra: &str) -> String {
    format!(
        "name: {name}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\ninference:\n  transport: http\n  endpoint: \
         {endpoint}\n  model: test-model\n  api_key: test-key\n  max_tokens: {MAX_OUTPUT}\n\
         {inference_extra}  driver:\n    artifact: {DRIVER}\n"
    )
}

fn session_ceiling(ceiling: u64) -> String {
    format!("  max_session_tokens: {ceiling}\n")
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

fn mur(home: &TempDir, project: &Project) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.dir.path());
    command
}

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

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir().join("out/result.txt")).unwrap_or_else(|err| {
            panic!(
                "no out/result.txt ({err}); stdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.workdir().join("trace.jsonl"))
    }

    fn warning_lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(&format!("warning[{code}]")))
            .collect()
    }
}

fn run_task(home: &TempDir, project: &Project) -> Run {
    Run::of(
        mur(home, project)
            .args([
                "run",
                "--manifest",
                project.manifest.to_str().unwrap(),
                "--task",
                "Say hello.",
                "--verbose",
            ])
            .output()
            .unwrap(),
    )
}

fn read_trace(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn tokens(inference: &Value) -> (u64, u64) {
    (
        inference["input_tokens"].as_u64().unwrap(),
        inference["output_tokens"].as_u64().unwrap(),
    )
}

/// The agent loop's own `inference` lines — a hook's carries `origin`.
fn agent_turns(trace: &[Value]) -> Vec<(u64, u64)> {
    events(trace, "inference")
        .into_iter()
        .filter(|event| event.get("origin").is_none())
        .map(tokens)
        .collect()
}

/// Every `inference` line's `input_tokens + output_tokens`, agent and hook alike.
fn trace_token_sum(trace: &[Value]) -> u64 {
    events(trace, "inference")
        .into_iter()
        .map(|event| {
            let (input, output) = tokens(event);
            input + output
        })
        .sum()
}

fn exit_status(trace: &[Value], event_type: &str) -> String {
    events(trace, event_type)
        .last()
        .unwrap_or_else(|| panic!("no {event_type} line"))["exit_status"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A ceiling the first call fits under and the second call does not: the first turn's input plus
/// its reserved output, plus half the first input as slack for the few tokens a fresh context id
/// moves the count by. The second turn's input alone exceeds that slack.
fn ceiling_between_first_and_second_call(first_input: u64) -> u64 {
    first_input + MAX_OUTPUT + first_input / 2
}

// ── S1–S3: the session ceiling ────────────────────────────────────────────────

#[test]
fn session_ceiling_not_reached() {
    println!("S1 session_ceiling_not_reached");
    let home = home_with_driver();
    let server = common::ScriptedServer::start(two_turns());
    let project = project(&http_manifest(
        "spend-capsule",
        &server.endpoint,
        &session_ceiling(1_000_000),
    ));
    let run = run_task(&home, &project);
    let trace = run.trace();

    assert_eq!(exit_status(&trace, "session_end"), "ok", "{}", run.stderr);
    assert_eq!(server.requests().len(), 2);
    assert!(events(&trace, "spend_ceiling_reached").is_empty());
    let start = events(&trace, "session_start")[0];
    assert_eq!(start["max_session_tokens"], json!(1_000_000));
    assert_eq!(start["machine_tokens_per_day"], Value::Null);
    assert!(!home.path().join(".murmur/spend").exists());
    println!("upstream requests: {}", server.requests().len());
}

#[test]
fn session_ceiling_stops_the_task() {
    println!("S2 session_ceiling_stops_the_task");
    let home = home_with_driver();

    let control_server = common::ScriptedServer::start(two_turns());
    // Bound, not passed inline: the project directory holds the session workdir the trace is
    // read from, and a temporary would delete it before the assertions run.
    let control_project = project(&http_manifest(
        "spend-capsule",
        &control_server.endpoint,
        &session_ceiling(1_000_000),
    ));
    let control = run_task(&home, &control_project);
    let control_turns = agent_turns(&control.trace());
    assert_eq!(control_turns.len(), 2, "{}", control.stderr);
    let (first_input, first_output) = control_turns[0];
    let (second_input, _) = control_turns[1];
    let ceiling = ceiling_between_first_and_second_call(first_input);
    assert!(ceiling >= first_input + MAX_OUTPUT);
    assert!(ceiling < first_input + first_output + second_input + MAX_OUTPUT);
    println!("control turns {control_turns:?}; ceiling {ceiling}");

    let server = common::ScriptedServer::start(two_turns());
    let stopped_project = project(&http_manifest(
        "spend-capsule",
        &server.endpoint,
        &session_ceiling(ceiling),
    ));
    let run = run_task(&home, &stopped_project);
    let trace = run.trace();
    let requests = server.requests().len();
    println!("upstream requests: {requests}");
    assert_eq!(requests, 1, "{}", run.stderr);

    let turns = agent_turns(&trace);
    assert_eq!(turns.len(), 1);
    let refusals = events(&trace, "spend_ceiling_reached");
    assert_eq!(refusals.len(), 1);
    let refusal = refusals[0];
    println!("refusal line: {refusal}");
    assert_eq!(refusal["limit"], "session");
    assert_eq!(refusal["ceiling"], json!(ceiling));
    let used = refusal["used"].as_u64().unwrap();
    assert_eq!(used, turns[0].0 + turns[0].1);
    assert!(refusal["requested"].as_u64().unwrap() > ceiling - used);
    assert!(refusal.get("origin").is_none());

    assert_eq!(exit_status(&trace, "task_end"), "spend_ceiling_reached");
    assert_eq!(exit_status(&trace, "session_end"), "spend_ceiling_reached");
    println!("task_end line: {}", events(&trace, "task_end")[0]);
    println!("session_end line: {}", events(&trace, "session_end")[0]);

    let result = run.result();
    println!("result: {result}");
    assert!(
        result.starts_with(&format!(
            "stopped: spend ceiling reached: inference.max_session_tokens is {ceiling}"
        )),
        "{result}"
    );
    assert!(result.contains("retrying will not get past it"), "{result}");
    for provider_error in [
        "driver invocation failed",
        "inference driver returned an error",
        "HttpRequestDenied",
    ] {
        assert!(!result.contains(provider_error), "{result}");
        assert!(!run.stderr.contains(provider_error), "{}", run.stderr);
    }
    assert!(trace_token_sum(&trace) <= ceiling);
    println!(
        "mur run exit: {:?}; stdout status: {:?}",
        run.output.status.code(),
        run.stdout.lines().find(|line| line.starts_with("status:"))
    );
}

#[test]
fn ceiling_below_first_call_sends_nothing() {
    println!("S3 ceiling_below_first_call_sends_nothing");
    let home = home_with_driver();
    let server = common::ScriptedServer::start(two_turns());
    let tiny_project = project(&http_manifest(
        "spend-capsule",
        &server.endpoint,
        &session_ceiling(10),
    ));
    let run = run_task(&home, &tiny_project);
    let trace = run.trace();
    assert_eq!(server.requests().len(), 0, "{}", run.stderr);
    assert!(events(&trace, "inference").is_empty());
    let refusals = events(&trace, "spend_ceiling_reached");
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0]["used"], json!(0));
    assert_eq!(exit_status(&trace, "session_end"), "spend_ceiling_reached");
    println!("upstream requests: 0; refusal line: {}", refusals[0]);
}

// ── S4: a latched session refuses the next task ───────────────────────────────

/// A scratch `$HOME` for the in-process launches below. Set once for the whole binary: the
/// subprocess cases above pass their own `HOME` explicitly.
fn scratch_home() {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        path
    });
}

fn stage_queue_capsule(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        assert!(matches!(artifact.runtime, ArtifactRuntime::Driver));
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    stage_session(
        std::sync::Arc::new(LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        )),
        StageRequest {
            credentials_file: None,
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: Some(LifecycleConfig {
                task_acceptance: TaskAcceptance::Queue,
                after_task: AfterTask::Sleep,
                queue_depth: 8,
                ..Default::default()
            }),
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

/// Launch on a background thread; returns the door address and the trace path.
fn launch(staged: capsule_runtime::StagedSession) -> (String, PathBuf) {
    let trace_path = staged.workdir.join("trace.jsonl");
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("timed out waiting for the capsule URL");
    (url, trace_path)
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

fn send_message(addr: &str, message_id: &str, text: &str) -> Value {
    http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {"messageId": message_id, "role": "user", "parts": [{"text": text}]}}
        })
        .to_string(),
    )
}

/// Send `text` over `message/stream` and read the connection to its end, returning the last
/// `status` event.
fn stream_message(addr: &str, message_id: &str, text: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("should connect for SSE");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {"message": {"messageId": message_id, "role": "user", "parts": [{"text": text}]}}
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
    let mut last_status = None;
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
                let status: Value = serde_json::from_str(rest).unwrap();
                let is_final = status["final"] == json!(true);
                last_status = Some(status);
                if is_final {
                    break;
                }
            }
        }
    }
    last_status.expect("the stream carried a status event")
}

fn wait_for_trace(path: &Path, what: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let trace = read_trace(path);
        if predicate(&trace) {
            return trace;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn in_process_project(endpoint: &str, ceiling: u64) -> PathBuf {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(
        &manifest,
        http_manifest("spend-queue", endpoint, &session_ceiling(ceiling)),
    )
    .unwrap();
    dir.keep().join("murmur.yaml")
}

#[test]
fn latched_session_refuses_the_next_task() {
    println!("S4 latched_session_refuses_the_next_task");
    scratch_home();
    let home = home_with_driver();

    // Control: the same capsule and the same task text, with room for both turns.
    let control_server = common::ScriptedServer::start(two_turns());
    let (control_url, control_trace) = launch(stage_queue_capsule(
        &home,
        &in_process_project(&control_server.endpoint, 1_000_000),
    ));
    send_message(&control_url, "msg-control", "go");
    let trace = wait_for_trace(&control_trace, "the control task_end", |trace| {
        !events(trace, "task_end").is_empty()
    });
    let first_input = agent_turns(&trace)[0].0;
    let ceiling = ceiling_between_first_and_second_call(first_input);

    let server = common::ScriptedServer::start(vec![
        tool_call("msg_1"),
        end_turn("msg_2", "never asked for"),
        end_turn("msg_3", "never asked for"),
    ]);
    let (url, trace_path) = launch(stage_queue_capsule(
        &home,
        &in_process_project(&server.endpoint, ceiling),
    ));

    send_message(&url, "msg-1", "go");
    let trace = wait_for_trace(&trace_path, "the first task_end", |trace| {
        !events(trace, "task_end").is_empty()
    });
    assert_eq!(server.requests().len(), 1);
    assert_eq!(exit_status(&trace, "task_end"), "spend_ceiling_reached");
    assert_eq!(events(&trace, "spend_ceiling_reached").len(), 1);

    let status = stream_message(&url, "msg-2", "go again");
    println!("second task terminal status: {status}");
    assert_eq!(status["status"]["state"], "failed", "{status}");
    assert!(
        status["status"]["message"]
            .as_str()
            .unwrap()
            .starts_with("spend ceiling reached:"),
        "{status}"
    );
    assert_eq!(status["final"], json!(true));

    let trace = wait_for_trace(&trace_path, "the second task_end", |trace| {
        events(trace, "task_end").len() == 2
    });
    assert_eq!(
        server.requests().len(),
        1,
        "the second task reached the provider"
    );
    let refusals = events(&trace, "spend_ceiling_reached");
    assert_eq!(refusals.len(), 2);
    let second_task = events(&trace, "task_start")[1];
    assert_eq!(refusals[1]["parent_id"], second_task["event_id"]);
    assert_eq!(refusals[1]["task_id"], second_task["task_id"]);
    assert_eq!(
        events(&trace, "task_end")[1]["exit_status"],
        "spend_ceiling_reached"
    );
    println!("upstream requests: {}", server.requests().len());
}

// ── S7, S12: the machine ceiling ──────────────────────────────────────────────

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn machine_ceiling_spans_processes() {
    println!("S7 machine_ceiling_spans_processes");
    let home = home_with_driver();

    // Control, with no machine ceiling in effect: counted by nothing, and writes no ledger.
    let control_server = common::ScriptedServer::start(vec![end_turn("msg_c", "hello")]);
    let control_project = project(&http_manifest(
        "spend-machine",
        &control_server.endpoint,
        "",
    ));
    let control = run_task(&home, &control_project);
    let first_input = agent_turns(&control.trace())[0].0;
    assert!(!home.path().join(".murmur/spend").exists());
    let ceiling = ceiling_between_first_and_second_call(first_input);
    set_machine_ceiling(&home, ceiling);

    let server_a = common::ScriptedServer::start(vec![end_turn("msg_a", "hello")]);
    let project_a = project(&http_manifest("spend-machine", &server_a.endpoint, ""));
    let run_a = run_task(&home, &project_a);
    let trace_a = run_a.trace();
    assert_eq!(
        exit_status(&trace_a, "session_end"),
        "ok",
        "{}",
        run_a.stderr
    );
    println!("A upstream requests: {}", server_a.requests().len());

    let spend_dir = home.path().join(".murmur/spend");
    assert_eq!(mode(&spend_dir), 0o700);
    let today = format!("{}.jsonl", chrono::Utc::now().format("%Y-%m-%d"));
    let ledger_path = spend_dir.join(&today);
    assert_eq!(mode(&ledger_path), 0o600);
    let ledger = fs::read_to_string(&ledger_path).unwrap();
    println!("ledger {today}:\n{}", ledger.trim_end());
    let a_session = events(&trace_a, "session_start")[0]["session_id"].clone();
    let a_turns = agent_turns(&trace_a);
    let lines: Vec<Value> = ledger
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), a_turns.len());
    for (line, (input, output)) in lines.iter().zip(&a_turns) {
        assert_eq!(line["session_id"], a_session);
        assert_eq!(line["input_tokens"], json!(input));
        assert_eq!(line["output_tokens"], json!(output));
    }
    let ledger_total: u64 = a_turns.iter().map(|(input, output)| input + output).sum();

    let server_b = common::ScriptedServer::start(vec![end_turn("msg_b", "hello")]);
    let project_b = project(&http_manifest("spend-machine", &server_b.endpoint, ""));
    let run_b = run_task(&home, &project_b);
    let trace_b = run_b.trace();
    println!("B upstream requests: {}", server_b.requests().len());
    assert_eq!(server_b.requests().len(), 0, "{}", run_b.stderr);
    let refusals = events(&trace_b, "spend_ceiling_reached");
    assert_eq!(refusals.len(), 1);
    println!("B refusal line: {}", refusals[0]);
    assert_eq!(refusals[0]["limit"], "machine");
    assert_eq!(refusals[0]["ceiling"], json!(ceiling));
    assert_eq!(refusals[0]["used"], json!(ledger_total));
    let result = run_b.result();
    assert!(
        result.starts_with(&format!(
            "stopped: spend ceiling reached: spend.machine_tokens_per_day is {ceiling}"
        )),
        "{result}"
    );
    assert_eq!(
        events(&trace_b, "session_start")[0]["machine_tokens_per_day"],
        json!(ceiling)
    );
}

#[test]
fn unusable_ledger_refuses_launch() {
    println!("S12 unusable_ledger_refuses_launch");
    let home = home_with_driver();
    set_machine_ceiling(&home, 1_000_000);
    fs::write(home.path().join(".murmur/spend"), "a file, not a directory").unwrap();

    let server = common::ScriptedServer::start(vec![end_turn("msg_1", "hello")]);
    let project = project(&http_manifest("spend-blocked", &server.endpoint, ""));
    let run = run_task(&home, &project);
    let combined = format!("{}{}", run.stdout, run.stderr);
    println!("{}", combined.trim());
    assert!(!run.output.status.success());
    let refusal = combined
        .lines()
        .find(|line| line.contains("error[E-RUN-026]"))
        .unwrap_or_else(|| panic!("{combined}"));
    assert!(refusal.contains(".murmur/spend"), "{refusal}");
    assert_eq!(server.requests().len(), 0);
    let workdirs = project.dir.path().join("workdir");
    assert!(
        fs::read_dir(&workdirs).map_or(true, |mut entries| entries.next().is_none()),
        "a session workdir was created"
    );
}

// ── S13: W-SEC-026 ────────────────────────────────────────────────────────────

#[test]
fn process_transport_warns_under_machine_ceiling() {
    println!("S13 process_transport_warns_under_machine_ceiling");
    let home = home_with_driver();
    set_machine_ceiling(&home, 1_000_000);

    let process = project(
        "name: spend-process\nversion: 0.1.0\nartifacts: []\ninference:\n  transport: process\n  \
         command: claude\n  model: test-model\n",
    );
    let http = project(&http_manifest("spend-http", "http://127.0.0.1:1", ""));

    for (label, project, expected) in [("process", &process, 1), ("http", &http, 0)] {
        let explained = Run::of(
            mur(&home, project)
                .args([
                    "run",
                    "--manifest",
                    project.manifest.to_str().unwrap(),
                    "--explain-scope",
                ])
                .output()
                .unwrap(),
        );
        let lines = explained.warning_lines("W-SEC-026");
        println!("{label} --explain-scope: {lines:?}");
        assert_eq!(lines.len(), expected, "{label}: {}", explained.stderr);

        let doctor = Run::of(mur(&home, project).arg("doctor").output().unwrap());
        let doctor_lines = doctor.warning_lines("W-SEC-026");
        println!("{label} doctor: {doctor_lines:?}");
        assert_eq!(doctor_lines.len(), expected, "{label}: {}", doctor.stderr);

        for line in lines.iter().chain(&doctor_lines) {
            assert!(line.contains(W_SEC_026_LINK), "{line}");
            assert!(line.contains("spend.machine_tokens_per_day"), "{line}");
            assert!(line.contains("transport: process"), "{line}");
        }
    }
}
