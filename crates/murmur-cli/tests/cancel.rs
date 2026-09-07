//! Stopping one running task without killing the capsule.
//!
//! Every case launches a real capsule in-process against a real Wasmtime driver and a scripted
//! provider, because the property under test is what the runtime stops doing: the acceptance
//! measurement is what the provider was asked for after the cancel, not what a command printed.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{
    load_runtime_manifest, ArtifactRuntime, ContainmentClass, ConversationMode, LocalRegistry,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const INPUT_TOOL_NAME: &str = "request-input-tool";
const INPUT_TOOL_VERSION: &str = "0.1.0";

/// A scratch `$HOME` for this whole test binary.
///
/// A launch staged in-process resolves the conversation record against the test process's own
/// `$HOME`, so without this the record cases would write into the developer's home. Set once,
/// never per test: `set_var` is process-global and every sibling test reads it.
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

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn_response(id: &str, text: &str) -> String {
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

/// A turn that asks for a tool, so the loop would make a second request if it were not stopped.
fn tool_call_response(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── Project setup ─────────────────────────────────────────────────────────────

/// A capsule that talks to `endpoint` through the fixture driver, with `extra` spliced into its
/// manifest — the capability, artifact and context blocks each case needs.
fn setup_project(endpoint: &str, name: &str, extra: &str) -> (TempDir, PathBuf) {
    scratch_home();
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    if extra.contains(INPUT_TOOL_NAME) {
        let tool_artifact = create_input_tool_artifact(artifacts.path());
        common::publish_local(&home, &tool_artifact).success();
    }

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             {extra}\
             inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
             api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn create_input_tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{INPUT_TOOL_NAME}-{INPUT_TOOL_VERSION}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let opts: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", opts).unwrap();
    writeln!(zip, "name: {INPUT_TOOL_NAME}").unwrap();
    writeln!(zip, "version: {INPUT_TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", opts).unwrap();
    zip.write_all(
        &fs::read(common::fixture_path(
            "input-required/tool/request-input-tool.wasm",
        ))
        .unwrap(),
    )
    .unwrap();

    zip.finish().unwrap();
    artifact_path
}

fn stage_agent(
    home: &TempDir,
    manifest_path: &Path,
    lifecycle: LifecycleConfig,
) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();

    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
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

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
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
            spawn_grant: None,
        },
    )
    .unwrap()
}

/// A queue capsule that sleeps between tasks — the only shape a cancel is interesting on, because
/// it is the only one where the session outlives the task.
fn queue_lifecycle() -> LifecycleConfig {
    LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: AfterTask::Sleep,
        queue_depth: 8,
        ..Default::default()
    }
}

/// One launched capsule under test, with the paths a case reads.
struct Capsule {
    url: String,
    trace_path: PathBuf,
    session_dir: PathBuf,
}

fn launch(staged: capsule_runtime::StagedSession) -> Capsule {
    let trace_path = staged.workdir.join("trace.jsonl");
    let session_dir = staged.workdir.clone();
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("timed out waiting for the capsule URL");
    Capsule {
        url,
        trace_path,
        session_dir,
    }
}

// ── Talking to the door ───────────────────────────────────────────────────────

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

fn send_message(addr: &str, message_id: &str, text: &str, context_id: Option<&str>) -> Value {
    let mut message = json!({
        "messageId": message_id,
        "role": "user",
        "parts": [{"text": text}]
    });
    if let Some(context_id) = context_id {
        message["contextId"] = json!(context_id);
    }
    http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": message}
        })
        .to_string(),
    )
}

fn tasks_get(addr: &str, task_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/get",
            "params": {"id": task_id}
        })
        .to_string(),
    )
}

fn tasks_cancel(addr: &str, task_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tasks/cancel",
            "params": {"id": task_id}
        })
        .to_string(),
    )
}

fn poll_until_state(addr: &str, task_id: &str, expected: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let response = tasks_get(addr, task_id);
        if response["result"]["status"]["state"].as_str() == Some(expected) {
            return response;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for state '{expected}'; last response: {response}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── Reading the trace ─────────────────────────────────────────────────────────

fn read_trace(trace_path: &Path) -> Vec<Value> {
    fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn events_named<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn wait_for_trace(
    trace_path: &Path,
    timeout: Duration,
    what: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = read_trace(trace_path);
        if predicate(&events) {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; trace holds: {:?}",
            events
                .iter()
                .map(|event| event["event_type"].as_str().unwrap_or("?").to_string())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Block until the scripted provider has recorded `count` requests.
fn wait_for_requests(server: &common::ScriptedServer, count: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while server.requests().len() < count {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} provider request(s); saw {}",
            server.requests().len()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every text a request's `messages` array carries, flattened, so a case can ask whether one
/// task's words reached the model on a later task's turn.
fn request_text(request: &Value) -> String {
    request
        .get("messages")
        .map(std::string::ToString::to_string)
        .unwrap_or_default()
}

fn mur() -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env_remove("NEXUS_API_KEY");
    cmd
}

// ── 1. The provider call stops ────────────────────────────────────────────────

/// The acceptance measurement: after a cancel, no further inference request is issued — counted
/// at the provider, not inferred from a command returning.
#[test]
fn cancel_stops_the_inference_call_in_flight() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "no-such-tool", json!({})),
            end_turn_response("msg_2", "a second turn that must never be asked for"),
        ],
        Duration::from_secs(4),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-agent", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let submitted = send_message(&capsule.url, "msg-1", "start something slow", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));

    let started = Instant::now();
    let canceled = tasks_cancel(&capsule.url, &task_id);
    let elapsed = started.elapsed();

    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the cancel waited out the provider call ({elapsed:?}); it must not wait for the loop"
    );

    // Longer than the scripted delay plus a whole further turn.
    std::thread::sleep(Duration::from_secs(10));
    assert_eq!(
        server.requests().len(),
        1,
        "the second scripted response was consumed: the loop asked the provider again after the \
         cancel"
    );
    assert_eq!(
        tasks_get(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
}

/// The manifest fragment allowing the capsule to reach its scripted provider.
fn network(server: &common::ScriptedServer) -> String {
    format!(
        "capabilities:\n  network:\n    allow:\n      - {}\n",
        server.endpoint
    )
}

// ── 2. The stream closes cleanly ──────────────────────────────────────────────

/// The final `canceled` status is the only thing that closes a `message/stream` connection on a
/// cancelled task, so a client's `for await` ends rather than hanging.
#[test]
fn cancel_closes_the_stream_with_a_final_canceled_status() {
    let server = common::ScriptedServer::start_with_delay(
        vec![end_turn_response("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-stream", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let stream = TcpStream::connect(&capsule.url).expect("should connect for SSE");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {"message": {"messageId": "msg-1", "role": "user", "parts": [{"text": "go"}]}}
    })
    .to_string();
    {
        let mut writer = &stream;
        writer
            .write_all(
                format!(
                    "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    capsule.url,
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        let _ = writer.flush();
    }
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut reader = BufReader::new(&stream);

    // Status line and headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }

    wait_for_requests(&server, 1, Duration::from_secs(30));
    let task_id = tasks_get_active_id(&capsule.url);
    let canceled = tasks_cancel(&capsule.url, &task_id);
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );

    // Read the connection to EOF and keep the last status event seen. The socket carries a read
    // timeout, so a server that neither sends nor closes surfaces as an error here rather than as
    // a test that hangs.
    let mut last_status: Option<Value> = None;
    let mut current_type = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            // The clean end: the server closed the connection after its final event.
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
        }
    }

    let last_status = last_status.expect("the stream carried at least one status event");
    assert_eq!(last_status["status"]["state"], "canceled", "{last_status}");
    assert_eq!(last_status["final"], json!(true), "{last_status}");
    assert_eq!(
        tasks_get(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
}

/// The id of whichever task holds the active slot — `message/stream` never reports one.
fn tasks_get_active_id(addr: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = http_post_json(
            addr,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tasks/get", "params": {}}).to_string(),
        );
        if let Some(id) = response["result"]["id"].as_str() {
            return id.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out discovering the active task id; last response: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ── 3. The session survives ───────────────────────────────────────────────────

/// A cancel ends one task and nothing else: the queued task behind it runs, and a task after that
/// still sees what both put in the conversation.
#[test]
fn cancel_leaves_the_session_and_its_queue_running() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn_response("msg_1", "task A, never delivered"),
            end_turn_response("msg_2", "task B is done"),
            end_turn_response("msg_3", "task C is done"),
        ],
        Duration::from_secs(3),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-queue", &network(&server));
    let capsule = launch(stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            conversation_mode: ConversationMode::Threaded,
            ..queue_lifecycle()
        },
    ));

    let context_id = "ctx_cancel_queue";
    let a = send_message(&capsule.url, "msg-a", "task A words", Some(context_id));
    let task_a = a["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));

    let b = send_message(&capsule.url, "msg-b", "task B words", Some(context_id));
    let task_b = b["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(b["result"]["status"]["state"], "submitted", "{b}");

    assert_eq!(
        tasks_cancel(&capsule.url, &task_a)["result"]["status"]["state"],
        "canceled"
    );

    let settled = poll_until_state(&capsule.url, &task_b, "completed", Duration::from_secs(60));
    assert_eq!(settled["result"]["status"]["state"], "completed");
    assert_eq!(
        tasks_get(&capsule.url, &task_a)["result"]["status"]["state"],
        "canceled"
    );

    let c = send_message(&capsule.url, "msg-c", "task C words", Some(context_id));
    let task_c = c["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(&capsule.url, &task_c, "completed", Duration::from_secs(60));

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "three tasks, three turns: {requests:?}");
    let third = request_text(&requests[2]);
    assert!(
        third.contains("task A words") && third.contains("task B words"),
        "task C's payload must carry what A and B put in the record: {third}"
    );

    // And the capsule is still answering for itself.
    let card = http_get(&capsule.url, "/.well-known/agent-card.json");
    assert!(card.contains("cancel-queue"), "{card}");
}

fn http_get(addr: &str, path: &str) -> String {
    let stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut writer = &stream;
    writer
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    let _ = writer.flush();
    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        response.push_str(&line);
        line.clear();
    }
    response
}

// ── 4 & 12. Residue ───────────────────────────────────────────────────────────

/// A shell-capable capsule whose first turn demotes a long command, launched and driven up to the
/// point where the demotion is on the record and the next turn's inference is in flight.
fn detached_scenario(name: &str) -> (common::ScriptedServer, TempDir, Capsule, String) {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "bash", json!({"command": "sleep 30"})),
            end_turn_response("msg_2", "never delivered"),
        ],
        Duration::from_secs(20),
    );
    let (home, manifest_path) = setup_project(
        &server.endpoint,
        name,
        &format!(
            "capabilities:\n  network:\n    allow:\n      - {}\n  shell:\n    allow:\n      - bash\n      - sleep\n",
            server.endpoint
        ),
    );
    let capsule = launch(stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            shell_grace_secs: 1,
            ..queue_lifecycle()
        },
    ));

    let submitted = send_message(&capsule.url, "msg-1", "start the long build", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_trace(
        &capsule.trace_path,
        Duration::from_secs(120),
        "the command to be demoted",
        |events| !events_named(events, "shell_detached").is_empty(),
    );
    (server, home, capsule, task_id)
}

/// Whether any process on this host is still running `sleep 30`.
fn a_sleep_is_still_running() -> bool {
    std::process::Command::new("ps")
        .args(["-eo", "args"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("sleep 30"))
        .unwrap_or(false)
}

/// The cancel names the demoted command by id and leaves it running. Nothing is killed.
#[test]
fn cancel_names_a_detached_shell_still_running() {
    if common::skip_without_host_support("cancel_names_a_detached_shell_still_running") {
        return;
    }
    let (_server, _home, capsule, task_id) = detached_scenario("cancel-residue");

    let canceled = tasks_cancel(&capsule.url, &task_id);
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );

    let artifacts = canceled["result"]["artifacts"]
        .as_array()
        .unwrap_or_else(|| panic!("a cancel with residue carries artifacts: {canceled}"));
    assert_eq!(artifacts.len(), 1, "{artifacts:?}");
    assert_eq!(artifacts[0]["name"], "residue");
    let parts = artifacts[0]["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");

    let item: Value = serde_json::from_str(parts[0]["text"].as_str().unwrap())
        .expect("every residue part is JSON");
    assert_eq!(item["kind"], "detached_shell", "{item}");
    assert_eq!(item["command"], "sleep 30", "{item}");
    let work_id = item["work_id"].as_str().unwrap();
    assert!(
        work_id.starts_with("wrk_")
            && !work_id["wrk_".len()..].is_empty()
            && work_id["wrk_".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "work id must be wrk_ plus lowercase hex: {work_id}"
    );

    assert!(
        a_sleep_is_still_running(),
        "the demoted command was killed; a cancel stops the runtime waiting, not the work"
    );
}

/// `mur cancel` is a thin caller: it names the task, the state, and one line per residue item.
#[test]
fn mur_cancel_reports_the_state_and_the_residue() {
    if common::skip_without_host_support("mur_cancel_reports_the_state_and_the_residue") {
        return;
    }
    let (_server, _home, capsule, task_id) = detached_scenario("cancel-cli-residue");

    let output = mur()
        .args(["cancel", &capsule.url, &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output).to_string();

    assert!(stdout.contains(&task_id), "{stdout}");
    assert!(stdout.contains("canceled"), "{stdout}");
    let residue_line = stdout
        .lines()
        .find(|line| line.starts_with("running:"))
        .unwrap_or_else(|| panic!("no residue line in:\n{stdout}"));
    assert!(residue_line.contains("wrk_"), "{residue_line}");
    assert!(residue_line.contains("sleep 30"), "{residue_line}");
}

// ── 5. No residue ─────────────────────────────────────────────────────────────

/// "Nothing else is running" is distinguishable from "these things are" without parsing an empty
/// list: the `artifacts` key is absent entirely.
#[test]
fn a_cancel_with_no_residue_omits_the_artifacts_key() {
    let server = common::ScriptedServer::start_with_delay(
        vec![end_turn_response("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-bare", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let submitted = send_message(&capsule.url, "msg-1", "nothing else running", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));

    let canceled = tasks_cancel(&capsule.url, &task_id);
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );
    assert!(
        canceled["result"].get("artifacts").is_none(),
        "a cancel with no residue must omit the key entirely: {canceled}"
    );
}

// ── 7. Cancelling from input-required ─────────────────────────────────────────

/// A task parked on a person's answer is cancellable, and lands on `canceled` rather than sitting
/// there until the input timeout turns it into a failure.
#[test]
fn cancel_from_input_required_reaches_canceled() {
    let server = common::ScriptedServer::start(vec![
        tool_call_response(
            "msg_1",
            "toolu_1",
            INPUT_TOOL_NAME,
            json!({"data": "Which branch?"}),
        ),
        end_turn_response("msg_2", "never delivered"),
    ]);
    let (home, manifest_path) = setup_project(
        &server.endpoint,
        "cancel-input",
        &format!(
            "  - name: {INPUT_TOOL_NAME}\n    version: {INPUT_TOOL_VERSION}\n    runtime: tool\n\
             capabilities:\n  network:\n    allow:\n      - {}\n",
            server.endpoint
        ),
    );
    let capsule = launch(stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            // Long enough that a task reaching `canceled` cannot be the timeout doing it.
            input_timeout_secs: Some(600),
            ..queue_lifecycle()
        },
    ));

    let submitted = send_message(&capsule.url, "msg-1", "ask me something", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(
        &capsule.url,
        &task_id,
        "input-required",
        Duration::from_secs(60),
    );

    let canceled = tasks_cancel(&capsule.url, &task_id);
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );
    let settled = poll_until_state(&capsule.url, &task_id, "canceled", Duration::from_secs(5));
    assert_eq!(settled["result"]["status"]["state"], "canceled");

    let events = wait_for_trace(
        &capsule.trace_path,
        Duration::from_secs(30),
        "the task to end",
        |events| !events_named(events, "task_end").is_empty(),
    );
    assert_eq!(
        events_named(&events, "task_end")[0]["exit_status"],
        "canceled"
    );
}

// ── 8. Terminal tasks and unknown ids ─────────────────────────────────────────

/// Cancelling a task that has already ended is a clean no-op with a `result`, and only an id this
/// capsule never held is an error.
#[test]
fn cancelling_a_terminal_task_is_a_clean_no_op() {
    let server = common::ScriptedServer::start(vec![
        end_turn_response("msg_1", "done"),
        end_turn_response("msg_2", "also done"),
    ]);
    let (home, manifest_path) =
        setup_project(&server.endpoint, "cancel-terminal", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let submitted = send_message(&capsule.url, "msg-1", "finish quickly", None);
    let done_task = submitted["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(
        &capsule.url,
        &done_task,
        "completed",
        Duration::from_secs(60),
    );

    for attempt in 0..2 {
        let response = tasks_cancel(&capsule.url, &done_task);
        assert!(
            response.get("error").is_none(),
            "attempt {attempt} answered with an error: {response}"
        );
        assert_eq!(
            response["result"]["status"]["state"], "completed",
            "attempt {attempt}: {response}"
        );
    }

    let unknown = tasks_cancel(&capsule.url, "tsk_doesnotexist");
    assert_eq!(unknown["error"]["code"], -32001, "{unknown}");
    assert_eq!(unknown["error"]["message"], "Task not found", "{unknown}");

    // A second cancel of an already-cancelled task is the same clean no-op.
    let server_two = common::ScriptedServer::start_with_delay(
        vec![end_turn_response("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let (home_two, manifest_two) =
        setup_project(&server_two.endpoint, "cancel-twice", &network(&server_two));
    let capsule_two = launch(stage_agent(&home_two, &manifest_two, queue_lifecycle()));
    let submitted = send_message(&capsule_two.url, "msg-1", "slow one", None);
    let slow_task = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server_two, 1, Duration::from_secs(30));
    assert_eq!(
        tasks_cancel(&capsule_two.url, &slow_task)["result"]["status"]["state"],
        "canceled"
    );
    let again = tasks_cancel(&capsule_two.url, &slow_task);
    assert!(again.get("error").is_none(), "{again}");
    assert_eq!(again["result"]["status"]["state"], "canceled", "{again}");
}

/// `mur cancel` against an unknown id fails with the door's own wording; against a completed task
/// it exits 0 and reports `completed`.
#[test]
fn mur_cancel_reports_unknown_and_completed_tasks() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "done")]);
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-cli", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let submitted = send_message(&capsule.url, "msg-1", "finish quickly", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(&capsule.url, &task_id, "completed", Duration::from_secs(60));

    let stdout = mur()
        .args(["cancel", &capsule.url, &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&stdout).to_string();
    assert!(stdout.contains(&task_id), "{stdout}");
    assert!(stdout.contains("completed"), "{stdout}");

    let failure = mur()
        .args(["cancel", &capsule.url, "tsk_doesnotexist"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8_lossy(&failure).contains("Task not found"),
        "{}",
        String::from_utf8_lossy(&failure)
    );
}

// ── 9. A task cancelled while queued ──────────────────────────────────────────

/// A cancel that lands before the task starts stops it from ever starting: no `task_start`, no
/// request to the provider, and a `task_canceled` record saying it was still queued.
#[test]
fn a_task_cancelled_while_queued_never_starts() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn_response("msg_1", "task A is done"),
            end_turn_response("msg_2", "a turn task B must never get"),
        ],
        Duration::from_secs(4),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-queued", &network(&server));
    let capsule = launch(stage_agent(&home, &manifest_path, queue_lifecycle()));

    let a = send_message(&capsule.url, "msg-a", "task A holds the slot", None);
    let task_a = a["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));

    let b = send_message(&capsule.url, "msg-b", "task B is never run", None);
    let task_b = b["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(b["result"]["status"]["state"], "submitted", "{b}");

    assert_eq!(
        tasks_cancel(&capsule.url, &task_b)["result"]["status"]["state"],
        "canceled"
    );

    poll_until_state(&capsule.url, &task_a, "completed", Duration::from_secs(60));
    let events = wait_for_trace(
        &capsule.trace_path,
        Duration::from_secs(60),
        "task B's cancellation record",
        |events| !events_named(events, "task_canceled").is_empty(),
    );

    assert_eq!(
        tasks_get(&capsule.url, &task_b)["result"]["status"]["state"],
        "canceled"
    );
    assert!(
        events_named(&events, "task_start")
            .iter()
            .all(|event| event["task_id"] != json!(task_b)),
        "a task cancelled while queued must never start: {events:?}"
    );
    let canceled = events_named(&events, "task_canceled");
    assert_eq!(canceled.len(), 1, "{canceled:?}");
    assert_eq!(canceled[0]["task_id"], json!(task_b), "{}", canceled[0]);
    assert_eq!(canceled[0]["phase"], "queued", "{}", canceled[0]);

    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request_text(request).contains("task B is never run")),
        "task B's message reached the provider: {:?}",
        server.requests()
    );
}

// ── 10. The conversation record ───────────────────────────────────────────────

/// The cancelled turn is appended to the record, marked, rather than silently truncated — and the
/// marker never reaches a driver.
#[test]
fn the_record_shows_the_cancelled_turn() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn_response("msg_1", "never delivered"),
            end_turn_response("msg_2", "the next task answers"),
        ],
        Duration::from_secs(4),
    );
    let record_store = "cancel-record-store";
    let (home, manifest_path) = setup_project(
        &server.endpoint,
        "cancel-record",
        &format!(
            "capabilities:\n  network:\n    allow:\n      - {}\ncontext:\n  record_store: {record_store}\n",
            server.endpoint
        ),
    );
    let capsule = launch(stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            conversation_mode: ConversationMode::Threaded,
            ..queue_lifecycle()
        },
    ));

    let context_id = "ctx_cancel_record";
    let submitted = send_message(&capsule.url, "msg-1", "stop me", Some(context_id));
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));
    assert_eq!(
        tasks_cancel(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
    poll_until_state(&capsule.url, &task_id, "canceled", Duration::from_secs(30));

    let record = scratch_home()
        .join(".murmur/conversations")
        .join(record_store)
        .join(context_id)
        .join("conversation.jsonl");
    let messages = wait_for_record(&record, Duration::from_secs(30), |messages| {
        messages
            .last()
            .is_some_and(|message| message.get("canceled").is_some())
    });
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "assistant", "{last}");
    assert_eq!(last["canceled"], json!(true), "{last}");
    let canceled_text = last["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("the cancelled turn carries text: {last}"))
        .to_string();

    // The next task on the same context succeeds, and carries the content but not the marker.
    let next = send_message(&capsule.url, "msg-2", "carry on", Some(context_id));
    let next_task = next["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(
        &capsule.url,
        &next_task,
        "completed",
        Duration::from_secs(60),
    );

    let requests = server.requests();
    let payload = request_text(&requests[requests.len() - 1]);
    assert!(
        payload.contains(&canceled_text),
        "the cancelled turn's content must reach the next task's payload: {payload}"
    );
    assert!(
        !payload.contains("\"canceled\""),
        "the runtime-owned marker must never reach a driver: {payload}"
    );
}

fn wait_for_record(
    path: &Path,
    timeout: Duration,
    predicate: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let messages: Vec<Value> = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if predicate(&messages) {
            return messages;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting on {}; it holds {} message(s)",
            path.display(),
            messages.len()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── 11. The trace ─────────────────────────────────────────────────────────────

/// Cancelled is not failed, in the record and in what `mur trace show` renders.
#[test]
fn the_trace_distinguishes_cancelled_from_failed() {
    let server = common::ScriptedServer::start_with_delay(
        vec![end_turn_response("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let (home, manifest_path) = setup_project(&server.endpoint, "cancel-trace", &network(&server));
    // `after_task: exit` so this launch ends once the cancelled task is over, which is both what
    // gives `mur trace show` a complete trace to read and the plainest demonstration that a
    // cancel changes nothing about how the loop decides what to do next.
    let capsule = launch(stage_agent(
        &home,
        &manifest_path,
        LifecycleConfig {
            after_task: AfterTask::Exit,
            ..queue_lifecycle()
        },
    ));

    let submitted = send_message(&capsule.url, "msg-1", "stop me mid-inference", None);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1, Duration::from_secs(30));
    assert_eq!(
        tasks_cancel(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );

    let events = wait_for_trace(
        &capsule.trace_path,
        Duration::from_secs(60),
        "the session to end after the cancelled task",
        |events| !events_named(events, "session_end").is_empty(),
    );

    let canceled = events_named(&events, "task_canceled");
    assert_eq!(canceled.len(), 1, "{canceled:?}");
    assert_eq!(canceled[0]["task_id"], json!(task_id), "{}", canceled[0]);
    assert_eq!(canceled[0]["phase"], "inference", "{}", canceled[0]);
    assert_eq!(canceled[0]["turn"], json!(0), "{}", canceled[0]);
    assert!(
        canceled[0]["detached_work_ids"].is_array() && canceled[0]["delegation_ids"].is_array(),
        "the record names the residue as arrays: {}",
        canceled[0]
    );

    let task_end = events_named(&events, "task_end");
    assert_eq!(task_end.len(), 1, "{task_end:?}");
    assert_eq!(task_end[0]["task_id"], json!(task_id), "{}", task_end[0]);
    assert_eq!(task_end[0]["exit_status"], "canceled", "{}", task_end[0]);

    // The session ended the way `after_task` said it would, reporting the task's own outcome.
    let session_end = events_named(&events, "session_end");
    assert_eq!(
        session_end[0]["exit_status"], "canceled",
        "{}",
        session_end[0]
    );

    let rendered = mur()
        .args(["trace", "show", capsule.trace_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rendered = String::from_utf8_lossy(&rendered).to_string();
    assert!(rendered.contains("task_canceled"), "{rendered}");
    assert!(rendered.contains("canceled"), "{rendered}");
    assert!(
        !rendered.contains("status:     failed"),
        "a cancelled task must not read as failed: {rendered}"
    );
    assert!(capsule.session_dir.exists());
}
