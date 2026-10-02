#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, LifecycleOverride, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()])
}

fn multi_turn_server(texts: &[&str]) -> common::ScriptedServer {
    let responses = texts
        .iter()
        .enumerate()
        .map(|(i, text)| {
            serde_json::json!({
                "id": format!("msg_{}", i + 1),
                "type": "message",
                "role": "assistant",
                "model": "test-model",
                "content": [{"type": "text", "text": text}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string()
        })
        .collect();
    common::ScriptedServer::start(responses)
}

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
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

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: lifecycle-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(
    home: &TempDir,
    manifest_path: &Path,
    lifecycle: Option<LifecycleConfig>,
    lifecycle_override: Option<LifecycleOverride>,
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
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
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
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle,
            lifecycle_override,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: runtime_manifest.exports.clone(),
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    http_post_json_with_headers(addr, path, body, &[])
}

/// The same POST with extra request headers, for the two provenance headers the peer door reads.
fn http_post_json_with_headers(
    addr: &str,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("\r\n{body}"));
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    let mut response_body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        response_body.push_str(&line);
    }
    serde_json::from_str(&response_body)
        .unwrap_or_else(|_| serde_json::json!({"_raw": response_body}))
}

fn message_send_body(message_id: &str, text: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": message_id,
                "role": "user",
                "parts": [{"text": text}]
            }
        }
    })
    .to_string()
}

/// Capsule with task_acceptance: none runs from task.md but HTTP message/send returns -32601.
#[test]
fn lifecycle_none_rejects_message_send() {
    let server = end_turn_server("none mode done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::None,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );
    // Write task.md so the agent loop runs (giving us a window to send HTTP requests)
    fs::write(staged.workdir.join("task.md"), "Run the task").unwrap();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let response = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("m-none", "should be rejected"),
    );
    assert_eq!(
        response["error"]["code"], -32601,
        "task_acceptance: none should reject message/send with -32601; got: {response}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Capsule with queue+sleep processes two queued tasks and stays alive indefinitely.
/// Termination is external (channel drop); idle timeout must NOT fire in this mode.
#[test]
fn lifecycle_queue_sleep_processes_two_tasks() {
    let server = multi_turn_server(&["task one done", "task two done"]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 2,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );
    let workdir_for_thread = staged.workdir.clone();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    // Enqueue both tasks before the agent processes either
    let r1 = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-1", "first task"),
    );
    assert_eq!(
        r1["result"]["status"]["state"], "submitted",
        "first task should be submitted; got: {r1}"
    );

    let r2 = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-2", "second task"),
    );
    assert_eq!(
        r2["result"]["status"]["state"], "submitted",
        "second task should be submitted; got: {r2}"
    );

    // Poll the trace file until both task_end events appear (max 30 s).
    // Do NOT rely on idle timeout — queue+sleep capsules wait indefinitely.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let trace = fs::read_to_string(workdir_for_thread.join("trace.jsonl")).unwrap_or_default();
        let task_end_count = trace.lines().filter(|l| l.contains("\"task_end\"")).count();
        if task_end_count >= 2 {
            let received_count = trace
                .lines()
                .filter(|l| l.contains("a2a_task_received"))
                .count();
            assert_eq!(
                received_count, 2,
                "trace should record 2 a2a_task_received events; got:\n{trace}"
            );
            assert_one_session_frame_over_two_tasks(&trace);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for both tasks to complete; trace:\n{}",
            fs::read_to_string(workdir_for_thread.join("trace.jsonl")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // The capsule is still alive (queue+sleep). Drop the join handle without waiting.
    drop(handle);
}

/// One launch, two queued tasks, one session frame: the `session_start` line precedes both
/// tasks and is the `parent_id` both `task_start` lines name. This capsule is `queue`+`sleep`
/// and never exits on its own, so nothing is asserted about `session_end` beyond it not having
/// closed the frame before the second task did.
fn assert_one_session_frame_over_two_tasks(trace: &str) {
    let events: Vec<serde_json::Value> = trace
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    let starts: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["event_type"] == "session_start")
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "a launch handling two tasks writes exactly one session_start; got:\n{trace}"
    );
    let session_node = starts[0]["event_id"].as_str().unwrap();

    let start_index = events
        .iter()
        .position(|e| e["event_type"] == "session_start")
        .unwrap();
    let task_starts: Vec<(usize, &serde_json::Value)> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e["event_type"] == "task_start")
        .collect();
    assert_eq!(task_starts.len(), 2, "both tasks must write task_start");
    for (i, ts) in &task_starts {
        assert!(
            start_index < *i,
            "session_start must precede every task_start"
        );
        assert_eq!(
            ts["parent_id"], session_node,
            "each task_start must parent to the session node"
        );
    }
    assert_ne!(
        task_starts[0].1["task_id"], task_starts[1].1["task_id"],
        "the two tasks must carry distinct task_ids"
    );

    let last_task_end = events
        .iter()
        .rposition(|e| e["event_type"] == "task_end")
        .unwrap();
    if let Some(end_index) = events.iter().position(|e| e["event_type"] == "session_end") {
        assert!(
            end_index > last_task_end,
            "session_end must not close the frame before the second task_end"
        );
    }
}

/// Five tasks, one lane apart: a `peer` task that arrives fourth runs second.
///
/// Task A is held by the scripted inference delay, so B, C, D and E all queue behind a running
/// task. E is the only one carrying peer headers, and it is the only one that jumps the queue —
/// past B, C and D, but not past A, which was already running.
#[test]
fn lifecycle_queue_runs_the_peer_lane_before_the_background_lane() {
    let server = common::ScriptedServer::start_with_delay(
        (1..=5)
            .map(|i| {
                serde_json::json!({
                    "id": format!("msg_{i}"),
                    "type": "message",
                    "role": "assistant",
                    "model": "test-model",
                    "content": [{"type": "text", "text": format!("task {i} done")}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })
                .to_string()
            })
            .collect(),
        std::time::Duration::from_secs(2),
    );
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    common::consent_to_peer_tasks(&manifest_path);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 8,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );
    let workdir_for_thread = staged.workdir.clone();
    let trace_path = workdir_for_thread.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let submitted = |response: &Value, label: &str| -> String {
        assert_eq!(
            response["result"]["status"]["state"], "submitted",
            "task {label} should be submitted; got: {response}"
        );
        response["result"]["id"]
            .as_str()
            .expect("a submitted task carries its id")
            .to_string()
    };

    // A goes in alone and starts running; the 2 s inference delay holds it there.
    let task_a = submitted(
        &http_post_json(&capsule_url, "/", &message_send_body("task-a", "task a")),
        "A",
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let trace = fs::read_to_string(&trace_path).unwrap_or_default();
        if trace.lines().any(|l| l.contains("\"task_start\"")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for task A to start; trace:\n{trace}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // B, C and D claim nothing, so they are untrusted events — the background lane. E claims
    // `peer`, the highest lane an inbound request can reach.
    let task_b = submitted(
        &http_post_json(&capsule_url, "/", &message_send_body("task-b", "task b")),
        "B",
    );
    let task_c = submitted(
        &http_post_json(&capsule_url, "/", &message_send_body("task-c", "task c")),
        "C",
    );
    let task_d = submitted(
        &http_post_json(&capsule_url, "/", &message_send_body("task-d", "task d")),
        "D",
    );
    let task_e = submitted(
        &http_post_json_with_headers(
            &capsule_url,
            "/",
            &message_send_body("task-e", "task e"),
            &[
                ("x-murmur-task-origin", "peer"),
                ("x-murmur-task-trust", "trusted"),
            ],
        ),
        "E",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let trace = loop {
        let trace = fs::read_to_string(&trace_path).unwrap_or_default();
        if trace.lines().filter(|l| l.contains("\"task_end\"")).count() >= 5 {
            break trace;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for all five tasks to complete; trace:\n{trace}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    let events: Vec<Value> = trace
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();
    let task_starts: Vec<&Value> = events
        .iter()
        .filter(|e| e["event_type"] == "task_start")
        .collect();

    let ran: Vec<&str> = task_starts
        .iter()
        .map(|e| e["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ran,
        vec![
            task_a.as_str(),
            task_e.as_str(),
            task_b.as_str(),
            task_c.as_str(),
            task_d.as_str()
        ],
        "the peer task must run after the one already running and before the queued \
         background ones; trace:\n{trace}"
    );

    let lanes: Vec<&str> = task_starts
        .iter()
        .map(|e| {
            e["lane"]
                .as_str()
                .expect("every task_start records its lane")
        })
        .collect();
    assert_eq!(
        lanes,
        vec!["bg", "peer", "bg", "bg", "bg"],
        "each task_start must name the lane it was selected from; trace:\n{trace}"
    );

    assert_no_two_tasks_overlap(&events, &trace);

    // The capsule is still alive (queue+sleep). Drop the join handle without waiting.
    drop(handle);
}

/// One active task at a time: in file order, every `task_start` after the first is preceded by
/// the previous task's `task_end`. A task that outranks the running one waits; it never
/// interleaves with it.
fn assert_no_two_tasks_overlap(events: &[Value], trace: &str) {
    let mut open: Option<&str> = None;
    for event in events {
        match event["event_type"].as_str() {
            Some("task_start") => {
                assert!(
                    open.is_none(),
                    "task {} started while {} was still running; trace:\n{trace}",
                    event["task_id"],
                    open.unwrap_or_default()
                );
                open = event["task_id"].as_str();
            }
            Some("task_end") => {
                assert_eq!(
                    open,
                    event["task_id"].as_str(),
                    "task_end must close the task that opened the frame; trace:\n{trace}"
                );
                open = None;
            }
            _ => {}
        }
    }
}

/// LifecycleOverride forces task_acceptance: none regardless of manifest default.
#[test]
fn lifecycle_override_forces_none() {
    let server = end_turn_server("override done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    // Manifest has no lifecycle (defaults to single+exit); override forces none
    let staged = stage_agent(
        &home,
        &manifest_path,
        None,
        Some(LifecycleOverride {
            task_acceptance: Some(TaskAcceptance::None),
            after_task: None,
        }),
    );
    fs::write(staged.workdir.join("task.md"), "Override task").unwrap();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("timed out waiting for capsule_url");

    let response = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("m-override", "should be rejected by override"),
    );
    assert_eq!(
        response["error"]["code"], -32601,
        "overridden-to-none lifecycle should reject message/send with -32601; got: {response}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Capsule with task_acceptance: none and no task.md exits immediately (no 30-second wait).
#[test]
fn lifecycle_none_exits_immediately_without_input() {
    // Server with no responses — the capsule must not make any inference calls
    let _server = common::ScriptedServer::start(vec![]);
    let (home, manifest_path) = setup_agent_project(&_server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::None,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            ..Default::default()
        }),
        None,
    );

    // No task.md written — capsule must exit without waiting

    let start = std::time::Instant::now();
    launch_session(staged, |_| {}).expect("launch should succeed");
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "task_acceptance: none with no task.md should exit in < 5s (got {elapsed:?})"
    );
}

// ── Detached shell completions ────────────────────────────────────────────────

/// `setup_agent_project` for a capsule that may run `bash` and `sleep`.
///
/// Nothing else is declared: on a kernel-enforcement host only the allowlisted binaries are
/// executable, so a command reaching for anything else fails rather than running.
fn setup_shell_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
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

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: lifecycle-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\n      - sleep\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn bash_call_response(id: &str, tool_use_id: &str, command: &str) -> String {
    serde_json::json!({
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
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn_response(id: &str, text: &str) -> String {
    serde_json::json!({
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

fn read_trace(trace_path: &Path) -> Vec<Value> {
    fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

/// Block until `predicate` holds over the trace, or fail naming what was there.
fn wait_for_trace(
    trace_path: &Path,
    seconds: u64,
    what: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let events = read_trace(trace_path);
        if predicate(&events) {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; trace holds: {:?}",
            events
                .iter()
                .map(|e| e["event_type"].as_str().unwrap_or("?").to_string())
                .collect::<Vec<_>>()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn events_named<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// A demoted command's result comes back as a `completion`-origin task in the `bg` lane, under
/// the context id of the conversation it was started from, and the trace joins the two ends.
#[test]
fn lifecycle_a_detached_command_completes_as_a_background_task() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_a_detached_command_completes_as_a_background_task",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        bash_call_response("msg_1", "toolu_build", "sleep 4; echo built"),
        end_turn_response("msg_2", "build started"),
        end_turn_response("msg_3", "noted the build result"),
    ]);
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 4,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    let trace_path = staged.workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    let response = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-build", "run the build"),
    );
    let first_task_id = response["result"]["id"]
        .as_str()
        .expect("a submitted task carries its id")
        .to_string();

    let events = wait_for_trace(&trace_path, 120, "the completion task to start", |events| {
        events_named(events, "task_start").len() >= 2
    });

    let starts = events_named(&events, "task_start");
    assert_eq!(starts[0]["task_id"], Value::String(first_task_id.clone()));
    let completion_start = starts[1];
    assert_eq!(completion_start["origin"], "completion");
    assert_eq!(completion_start["lane"], "bg");
    assert_eq!(completion_start["source"], "detached_shell");
    assert_eq!(
        completion_start["context_id"], starts[0]["context_id"],
        "the completion joins the conversation the command was started from"
    );

    let detached = events_named(&events, "shell_detached");
    assert_eq!(detached.len(), 1);
    let completed = events_named(&events, "shell_completed");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["work_id"], detached[0]["work_id"]);
    assert_eq!(
        completed[0]["completion_task_id"], completion_start["task_id"],
        "shell_completed names the task it enqueued"
    );
    assert_eq!(completed[0]["exit_code"], 0);
    assert_eq!(completed[0]["status"], "ok");
    assert_eq!(
        completed[0]["output_path"],
        Value::String(format!(
            "logs/{}.log",
            detached[0]["work_id"].as_str().unwrap()
        ))
    );
}

/// A person's request is not made to wait behind a completion: the peer task runs first even
/// though the completion was pushed onto the queue before it.
#[test]
fn lifecycle_a_completion_waits_behind_a_peer_request() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_a_completion_waits_behind_a_peer_request",
    ) {
        return;
    }
    // Every response is held for two seconds, so the first task is still running when both the
    // peer request and the detached command's completion land.
    let server = common::ScriptedServer::start_with_delay(
        vec![
            bash_call_response("msg_1", "toolu_build", "sleep 2; echo built"),
            bash_call_response("msg_2", "toolu_echo", "echo second"),
            end_turn_response("msg_3", "first task done"),
            end_turn_response("msg_4", "peer task done"),
            end_turn_response("msg_5", "completion task done"),
        ],
        std::time::Duration::from_secs(2),
    );
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);
    common::consent_to_peer_tasks(&manifest_path);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 8,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    let trace_path = staged.workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    let first = http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-first", "run the build"),
    );
    let first_task_id = first["result"]["id"].as_str().unwrap().to_string();

    // The peer request is delivered while the first task is still in its opening turn, so it is
    // waiting in the channel before the completion is ever drained.
    wait_for_trace(&trace_path, 60, "the first task to start", |events| {
        !events_named(events, "task_start").is_empty()
    });
    let peer = http_post_json_with_headers(
        &capsule_url,
        "/",
        &message_send_body("task-peer", "a peer is asking"),
        &[
            ("x-murmur-task-origin", "peer"),
            ("x-murmur-task-trust", "trusted"),
        ],
    );
    let peer_task_id = peer["result"]["id"].as_str().unwrap().to_string();

    let events = wait_for_trace(&trace_path, 180, "all three tasks to start", |events| {
        events_named(events, "task_start").len() >= 3
    });
    let starts = events_named(&events, "task_start");

    assert_eq!(starts[0]["task_id"], Value::String(first_task_id));
    assert_eq!(starts[1]["task_id"], Value::String(peer_task_id));
    assert_eq!(starts[1]["origin"], "peer");
    assert_eq!(
        starts[2]["origin"], "completion",
        "the completion runs last, behind the peer request"
    );
    assert_eq!(starts[2]["lane"], "bg");
}

/// The abandonment report block in `logs/bootstrap.log`, or `None` if the sweep wrote none.
///
/// Read from `bootstrap.log` rather than `trace.jsonl` on purpose: under `after_task: exit` there
/// is no turn left to tell the agent in, so the operator's surfaces are the whole delivery.
fn abandonment_report(workdir: &Path) -> Option<String> {
    let log = fs::read_to_string(workdir.join("logs").join("bootstrap.log")).ok()?;
    let start = log.find("background shell command")?;
    Some(log[start..].to_string())
}

/// Block until `logs/bootstrap.log` holds an abandonment report, or fail naming what it does hold.
fn wait_for_abandonment_report(workdir: &Path, seconds: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        if let Some(report) = abandonment_report(workdir) {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for an abandonment report; logs/bootstrap.log holds: {}",
            fs::read_to_string(workdir.join("logs").join("bootstrap.log")).unwrap_or_default()
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Under `after_task: exit` a command still running at session end is stated to the operator in
/// full — the work id, the binary, the command text as the model wrote it, and how long it had
/// been running — on a surface that is not `trace.jsonl`.
#[test]
fn lifecycle_a_command_still_running_at_exit_is_reported_to_the_operator() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_a_command_still_running_at_exit_is_reported_to_the_operator",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        bash_call_response("msg_1", "toolu_build", "sleep 45; echo done"),
        end_turn_response("msg_2", "build started"),
    ]);
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Exit,
            queue_depth: 4,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    let workdir = staged.workdir.clone();
    let trace_path = workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-build", "run the build"),
    );

    let events = wait_for_trace(&trace_path, 120, "the command to be demoted", |events| {
        !events_named(events, "shell_detached").is_empty()
    });
    let work_id = events_named(&events, "shell_detached")[0]["work_id"]
        .as_str()
        .expect("a demoted command carries its work id")
        .to_string();

    let report = wait_for_abandonment_report(&workdir, 120);
    assert!(report.contains(&work_id), "report was: {report}");
    assert!(report.contains("bash"), "report was: {report}");
    assert!(
        report.contains("command: sleep 45; echo done"),
        "the report names the command as the model wrote it; report was: {report}"
    );
    assert!(
        report.contains("state: still running after"),
        "the report says how long it had been running; report was: {report}"
    );
    assert!(
        report.contains(&format!("no logs/{work_id}.log will be written")),
        "the report must not promise a file that never appears; report was: {report}"
    );
}

/// A command that finishes inside the grace period is untouched on every surface: no
/// `shell_detached`, no `shell_abandoned`, and no abandonment report block. Demotion is what the
/// abandonment machinery hangs off, so a command that never demotes must reach none of it.
#[test]
fn lifecycle_a_command_inside_the_grace_period_is_reported_nowhere() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_a_command_inside_the_grace_period_is_reported_nowhere",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        bash_call_response("msg_1", "toolu_quick", "echo quick"),
        end_turn_response("msg_2", "ran it"),
    ]);
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Exit,
            queue_depth: 4,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    let workdir = staged.workdir.clone();
    let trace_path = workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-quick", "run the quick one"),
    );

    let events = wait_for_trace(&trace_path, 120, "the session to end", |events| {
        !events_named(events, "session_end").is_empty()
    });

    assert_eq!(
        events_named(&events, "shell").len(),
        1,
        "the command ran in the foreground and its output went to the turn"
    );
    assert!(events_named(&events, "shell_detached").is_empty());
    assert!(events_named(&events, "shell_abandoned").is_empty());
    assert_eq!(
        abandonment_report(&workdir),
        None,
        "nothing was discarded, so nothing is reported"
    );
}

/// A command that finishes during teardown — after the task loop stopped reading completions and
/// before the sweep drains the channel — is reported with what is known about it, not flattened
/// into "still running, result lost".
///
/// The window is opened by holding every scripted response: the command is demoted a second after
/// it spawns, finishes a second after that, and the end-turn response that breaks the task loop is
/// still in flight for a second more. The sweep therefore finds nothing outstanding and one
/// completion waiting on the channel.
#[test]
fn lifecycle_a_command_that_finished_during_teardown_is_reported_with_its_result() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_a_command_that_finished_during_teardown_is_reported_with_its_result",
    ) {
        return;
    }
    let server = common::ScriptedServer::start_with_delay(
        vec![
            bash_call_response("msg_1", "toolu_build", "sleep 2; echo done"),
            end_turn_response("msg_2", "build started"),
        ],
        std::time::Duration::from_secs(3),
    );
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Exit,
            queue_depth: 4,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    let workdir = staged.workdir.clone();
    let trace_path = workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    http_post_json(
        &capsule_url,
        "/",
        &message_send_body("task-build", "run the build"),
    );

    let report = wait_for_abandonment_report(&workdir, 180);
    assert!(
        report.contains("state: finished during teardown after"),
        "report was: {report}"
    );
    assert!(report.contains("status: ok"), "report was: {report}");
    assert!(report.contains("exit_code: 0"), "report was: {report}");

    let events = wait_for_trace(&trace_path, 60, "the abandonment record", |events| {
        !events_named(events, "shell_abandoned").is_empty()
    });
    let detached = events_named(&events, "shell_detached");
    let abandoned = events_named(&events, "shell_abandoned");
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0]["work_id"], detached[0]["work_id"]);
    assert_eq!(abandoned[0]["exit_code"], 0);
    let work_id = detached[0]["work_id"].as_str().unwrap();
    assert_eq!(
        abandoned[0]["output_path"],
        Value::String(format!("logs/{work_id}.log"))
    );
    assert!(
        abandoned[0]["output_bytes"].as_u64().unwrap() > 0,
        "the output log was written before the sweep ran: {}",
        abandoned[0]
    );
    assert!(
        workdir.join("logs").join(format!("{work_id}.log")).exists(),
        "the report names a file that is on disk"
    );
    assert!(
        events_named(&events, "shell_completed").is_empty(),
        "no task carried the result, so nothing wrote shell_completed"
    );
}

// ── Ending without an empty prompt ────────────────────────────────────────────

/// `mur run --manifest <manifest> --verbose <args>` as a child process, with only `env` in its
/// idle-timeout environment: an inherited `MURMUR_A2A_TIMEOUT_SECS` is removed first, so a test
/// that sets none runs against the 30-second default.
fn mur_run(
    home: &TempDir,
    manifest_path: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = assert_cmd::Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("MURMUR_A2A_TIMEOUT_SECS");
    for (key, value) in env {
        command.env(key, value);
    }
    command
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--verbose",
        ])
        .args(args)
        .output()
        .expect("mur run should execute")
}

/// The text of the last `user` message in a recorded driver request.
fn last_user_text(request: &Value) -> String {
    let last_user = request["messages"]
        .as_array()
        .and_then(|messages| messages.iter().rev().find(|m| m["role"] == "user"))
        .unwrap_or_else(|| panic!("the request carries a user message: {request}"));
    match &last_user["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("unexpected user content: {other}"),
    }
}

/// Milliseconds from `task_end` to `session_end`, which is how long the session held on after its
/// task. Read off the trace rather than a clock around the process, so the time spent compiling
/// the capsule before the task can neither hide nor fake a wait.
fn millis_from_task_end_to_session_end(events: &[Value]) -> u64 {
    let task_end = events_named(events, "task_end")[0]["timestamp"]
        .as_u64()
        .unwrap();
    let session_end = events_named(events, "session_end")[0]["timestamp"]
        .as_u64()
        .unwrap();
    session_end - task_end
}

/// `queue` + `exit` given its task with `--task` ends as soon as that task does: no idle wait, no
/// second request, and no turn or token beyond the task's own.
#[test]
fn lifecycle_queue_exit_ends_as_soon_as_its_task_md_task_does() {
    // A second response is scripted so that a second request would be recorded rather than
    // refused.
    let server = multi_turn_server(&["done", "an answer to nothing"]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let output = mur_run(
        &home,
        &manifest_path,
        &[
            "--task",
            "Say done.",
            "--lifecycle-task-acceptance",
            "queue",
            "--lifecycle-after-task",
            "exit",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "mur run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let workdir = common::parse_workdir_from_stdout(&stdout);
    let events = read_trace(&workdir.join("trace.jsonl"));
    let held = millis_from_task_end_to_session_end(&events);
    assert!(
        held < 5_000,
        "the session held on {held}ms after its task ended, so it waited out the idle timeout"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one task, one request: {requests:?}");
    assert!(
        !last_user_text(&requests[0]).trim().is_empty(),
        "the request carries the task: {}",
        requests[0]
    );
    assert!(
        !stderr.contains("no A2A message received"),
        "the session never waited for a message:\n{stderr}"
    );

    assert_eq!(events_named(&events, "task_start").len(), 1);
    let task_ends = events_named(&events, "task_end");
    assert_eq!(task_ends.len(), 1);
    assert_eq!(events_named(&events, "inference").len(), 1);
    let session_end = events_named(&events, "session_end")[0];
    assert_eq!(session_end["exit_status"], "ok");
    assert_eq!(session_end["total_turns"], 1);
    assert_eq!(
        session_end["total_input_tokens"], task_ends[0]["input_tokens"],
        "the session spent nothing beyond its task"
    );
}

/// Ending straight after a `task.md` task still runs the teardown sweep: a demoted command is
/// discarded and reported to the operator, not handed to a task nobody waits for.
#[test]
fn lifecycle_queue_exit_task_md_still_reports_discarded_shell_work() {
    if capsule_runtime::skip_without_host_support(
        "lifecycle_queue_exit_task_md_still_reports_discarded_shell_work",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        bash_call_response("msg_1", "toolu_build", "sleep 45; echo done"),
        end_turn_response("msg_2", "build started"),
        end_turn_response("msg_3", "an answer to nothing"),
    ]);
    let (home, manifest_path) = setup_shell_agent_project(&server.endpoint);

    let staged = stage_agent(
        &home,
        &manifest_path,
        Some(LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Exit,
            queue_depth: 4,
            shell_grace_secs: 1,
            ..Default::default()
        }),
        None,
    );
    fs::write(staged.accessible_workdir.join("task.md"), "Run the build.").unwrap();
    let workdir = staged.workdir.clone();
    let trace_path = workdir.join("trace.jsonl");

    std::thread::spawn(move || {
        let _ = launch_session(staged, |_| {});
    });

    let events = wait_for_trace(&trace_path, 180, "the session to end", |events| {
        !events_named(events, "session_end").is_empty()
    });
    let work_id = events_named(&events, "shell_detached")[0]["work_id"]
        .as_str()
        .expect("a demoted command carries its work id")
        .to_string();

    let report = wait_for_abandonment_report(&workdir, 60);
    assert!(report.contains(&work_id), "report was: {report}");
    assert!(report.contains("bash"), "report was: {report}");
    assert!(
        report.contains("command: sleep 45; echo done"),
        "report was: {report}"
    );
    assert!(
        report.contains("state: still running after"),
        "report was: {report}"
    );
    assert!(
        events_named(&events, "shell_abandoned")
            .iter()
            .any(|event| event["work_id"] == work_id.as_str()),
        "the discarded command has its shell_abandoned record"
    );

    let held = millis_from_task_end_to_session_end(&events);
    assert!(
        held < 5_000,
        "the session held on {held}ms after its task ended"
    );
    assert_eq!(
        server.requests().len(),
        2,
        "the task's two turns, and nothing for the completion"
    );
}

/// A launch that is never given a task waits the idle window for one and then ends without
/// calling the model, on the default lifecycle and on `queue` + `exit` alike.
#[test]
fn lifecycle_a_launch_that_receives_no_task_ends_without_calling_the_model() {
    for lifecycle_args in [
        &[][..],
        &[
            "--lifecycle-task-acceptance",
            "queue",
            "--lifecycle-after-task",
            "exit",
        ][..],
    ] {
        // One response is scripted so that a request would be recorded rather than refused.
        let server = end_turn_server("an answer to nothing");
        let (home, manifest_path) = setup_agent_project(&server.endpoint);

        let output = mur_run(
            &home,
            &manifest_path,
            lifecycle_args,
            &[("MURMUR_A2A_TIMEOUT_SECS", "2")],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{lifecycle_args:?}: mur run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("ending the session without calling the model"),
            "{lifecycle_args:?}: stderr was:\n{stderr}"
        );
        assert_eq!(
            server.requests().len(),
            0,
            "{lifecycle_args:?}: nothing was sent to the model"
        );

        let workdir = common::parse_workdir_from_stdout(&stdout);
        let events = read_trace(&workdir.join("trace.jsonl"));
        assert!(events_named(&events, "inference").is_empty());
        assert!(events_named(&events, "task_start").is_empty());
        let session_end = events_named(&events, "session_end");
        assert_eq!(session_end.len(), 1);
        assert_eq!(session_end[0]["exit_status"], "ok");
    }
}

/// A `--task` of nothing but whitespace fails its task before any request is made.
#[test]
fn lifecycle_a_blank_task_is_never_sent_to_the_model() {
    // One response is scripted so that a request would be recorded rather than refused.
    let server = end_turn_server("an answer to nothing");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let output = mur_run(&home, &manifest_path, &["--task", "   "], &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a blank task must fail the run:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("the task is empty"),
        "stderr was:\n{stderr}"
    );
    assert_eq!(server.requests().len(), 0, "nothing was sent to the model");

    let workdir = common::parse_workdir_from_stdout(&stdout);
    let events = read_trace(&workdir.join("trace.jsonl"));
    assert_eq!(events_named(&events, "task_start").len(), 1);
    let task_ends = events_named(&events, "task_end");
    assert_eq!(task_ends.len(), 1);
    assert_eq!(task_ends[0]["exit_status"], "failed");
    assert!(events_named(&events, "inference").is_empty());
}

/// An empty message from an untrusted peer fails its task before any request is made. The fence
/// an untrusted payload is wrapped in is never empty, so this holds only because the check reads
/// the task text before fencing.
#[test]
fn lifecycle_a_blank_untrusted_message_is_never_sent_to_the_model() {
    // One response is scripted so that a request would be recorded rather than refused.
    let server = end_turn_server("an answer to nothing");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    common::consent_to_peer_tasks(&manifest_path);

    let staged = stage_agent(&home, &manifest_path, None, None);
    let trace_path = staged.workdir.join("trace.jsonl");

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");

    http_post_json_with_headers(
        &capsule_url,
        "/",
        &message_send_body("m-blank", ""),
        &[
            ("x-murmur-task-origin", "peer"),
            ("x-murmur-task-trust", "untrusted"),
        ],
    );

    let result = handle.join().expect("launch thread should not panic");
    let error = result.expect_err("a failed task fails the launch");
    assert!(
        error.to_string().contains("the task is empty"),
        "error was: {error}"
    );

    let events = read_trace(&trace_path);
    let task_starts = events_named(&events, "task_start");
    assert_eq!(task_starts.len(), 1);
    assert_eq!(task_starts[0]["trust"], "untrusted");
    let task_ends = events_named(&events, "task_end");
    assert_eq!(task_ends.len(), 1);
    assert_eq!(task_ends[0]["exit_status"], "failed");
    assert!(events_named(&events, "inference").is_empty());
    assert_eq!(server.requests().len(), 0, "nothing was sent to the model");
}

// ── Tasks still queued when the session stops taking work ─────────────────────

/// The `status.message` of a task refused because the session ended before it started.
const REJECTED_SESSION_ENDED_MESSAGE: &str =
    "task rejected: the session ended before this task started";

/// A body the fixture driver fails the task on: another provider's response shape.
const CHAT_COMPLETIONS_BODY: &str = r#"{"id":"x","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;

/// A provider answering its one request with `body`, and only once the test releases it. The
/// first receiver fires when the request arrives, so the test knows the `task.md` task is running
/// and not yet finished.
fn held_provider(
    body: String,
) -> (
    common::ScriptedServer,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let server = common::ScriptedServer::start_answering(1, move |_| {
        let _ = arrived_tx.send(());
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(60));
        body.clone()
    });
    (server, arrived_rx, release_tx)
}

/// `launch_session` on a thread, and the capsule URL it announced.
fn launch_in_background(
    staged: capsule_runtime::StagedSession,
) -> (
    std::thread::JoinHandle<Result<capsule_runtime::LaunchResult, capsule_runtime::RuntimeError>>,
    String,
) {
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("timed out waiting for capsule_url");
    (handle, capsule_url)
}

/// A `queue` + `exit` capsule with room for four queued tasks.
fn queue_exit_lifecycle() -> LifecycleConfig {
    LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: AfterTask::Exit,
        queue_depth: 4,
        input_timeout_secs: None,
        ..Default::default()
    }
}

/// `message/send` of `text`, returning the task id and the state it was answered with.
fn send_task(addr: &str, message_id: &str, text: &str) -> (String, String) {
    let response = http_post_json(addr, "/", &message_send_body(message_id, text));
    let task_id = response["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("message/send returned no task id: {response}"))
        .to_string();
    let state = response["result"]["status"]["state"]
        .as_str()
        .unwrap_or_else(|| panic!("message/send returned no state: {response}"))
        .to_string();
    (task_id, state)
}

/// Open a `message/stream` for `text` and read it until the server closes it, returning every
/// `status` frame's data in order. `headers_tx` fires once the response headers have arrived,
/// which the door writes directly before it enqueues the task.
fn stream_until_closed(
    addr: String,
    text: &str,
    headers_tx: std::sync::mpsc::Sender<()>,
) -> std::thread::JoinHandle<Vec<Value>> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/stream",
        "params": {"message": {"messageId": "m-stream", "role": "user", "parts": [{"text": text}]}}
    })
    .to_string();
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect(&addr).expect("should connect for SSE");
        stream
            .write_all(
                format!(
                    "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(90)))
            .unwrap();
        let mut reader = BufReader::new(&stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
        }
        let _ = headers_tx.send(());
        let mut statuses = Vec::new();
        let mut current_type = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Err(error) => panic!("the SSE connection errored instead of closing: {error}"),
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(rest) = line.strip_prefix("event: ") {
                current_type = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if current_type == "status" {
                    statuses.push(serde_json::from_str(rest).expect("a status frame is JSON"));
                }
            }
        }
        statuses
    })
}

/// Every trace event that names `task_id`.
fn events_naming<'a>(events: &'a [Value], task_id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["task_id"] == task_id)
        .collect()
}

/// Asserts `task_id`'s only trace record is one `task_rejected` with `cause`, parented to the
/// session node, and returns it.
fn assert_sole_rejection<'a>(events: &'a [Value], task_id: &str, cause: &str) -> &'a Value {
    let named = events_naming(events, task_id);
    let kinds: Vec<&str> = named
        .iter()
        .map(|event| event["event_type"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        kinds,
        ["task_rejected"],
        "a refused task has one record and it is task_rejected: {named:?}"
    );
    let rejected = named[0];
    assert_eq!(rejected["cause"], cause, "{rejected}");
    assert_eq!(rejected["source"], "a2a", "{rejected}");
    assert!(
        rejected["context_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{rejected}"
    );
    let session_start = events_named(events, "session_start")[0];
    assert_eq!(
        rejected["parent_id"], session_start["event_id"],
        "a refused task hangs off the session: {rejected}"
    );
    rejected
}

/// A task queued behind a `queue` + `exit` capsule's own `task.md` task is refused when that
/// task closes the session: it never runs, it reads `rejected` everywhere, and the launch's own
/// outcome is the `task.md` task's.
#[test]
fn lifecycle_a_task_queued_behind_an_exit_task_is_rejected_when_the_session_closes() {
    let (server, arrived, release) = held_provider(end_turn_response("msg_1", "done"));
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path, Some(queue_exit_lifecycle()), None);
    fs::write(staged.accessible_workdir.join("task.md"), "Say done.").unwrap();
    let trace_path = staged.workdir.join("trace.jsonl");
    let session_id = staged.session_id.clone();
    let sessions_root = staged.workdir.parent().unwrap().to_path_buf();

    let (handle, capsule_url) = launch_in_background(staged);
    arrived
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the task.md task reached the provider");

    let (task_b, state) = send_task(&capsule_url, "m-b", "queued behind the launch task");
    assert_eq!(state, "submitted");
    let (headers_tx, headers_rx) = std::sync::mpsc::channel();
    let stream_c = stream_until_closed(capsule_url.clone(), "streamed behind it", headers_tx);
    headers_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the stream was answered");
    // The door enqueues directly after writing the headers, with no await in between.
    std::thread::sleep(std::time::Duration::from_millis(300));
    release.send(()).unwrap();

    let result = handle.join().expect("launch thread should not panic");
    assert!(result.is_ok(), "the launch task completed: {result:?}");
    let statuses = stream_c.join().expect("stream thread should not panic");

    let events = read_trace(&trace_path);
    let task_starts = events_named(&events, "task_start");
    assert_eq!(task_starts.len(), 1, "only the task.md task ran");
    assert_eq!(task_starts[0]["source"], "task_md");
    let task_ends = events_named(&events, "task_end");
    assert_eq!(task_ends.len(), 1);
    assert_eq!(task_ends[0]["task_id"], task_starts[0]["task_id"]);

    let rejections = events_named(&events, "task_rejected");
    assert_eq!(rejections.len(), 2, "{rejections:?}");
    let task_c = rejections
        .iter()
        .map(|event| event["task_id"].as_str().unwrap().to_string())
        .find(|task_id| *task_id != task_b)
        .expect("the streamed task was refused too");
    for task_id in [&task_b, &task_c] {
        assert_sole_rejection(&events, task_id, "session_ended");
    }
    assert!(events_named(&events, "a2a_task_received").is_empty());
    assert_eq!(server.requests().len(), 1, "only the task.md task was sent");
    assert_eq!(events_named(&events, "session_end")[0]["exit_status"], "ok");

    let final_c = statuses
        .iter()
        .rfind(|status| status["final"] == true)
        .unwrap_or_else(|| panic!("the stream carried no final status: {statuses:?}"));
    assert_eq!(final_c["id"], task_c.as_str(), "{statuses:?}");
    assert_eq!(final_c["status"]["state"], "rejected");
    assert_eq!(final_c["status"]["message"], REJECTED_SESSION_ENDED_MESSAGE);

    let shown = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "show", &session_id, "--workdir"])
        .arg(&sessions_root)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(stdout.contains("Rejected"), "{stdout}");
    assert!(stdout.contains(&task_b), "{stdout}");
    assert!(stdout.contains(&task_c), "{stdout}");
}

/// A refused task reads `rejected` over `tasks/get` for as long as the door is up, a cancel of it
/// is a cancel of any ended task, and the closed door refuses new work without recording it.
///
/// The `on-session-end` hook spins until its deadline, which holds teardown — and so the door —
/// open after the refusal.
#[test]
fn lifecycle_a_rejected_task_reads_rejected_over_tasks_get() {
    let (server, arrived, release) = held_provider(end_turn_response("msg_1", "done"));
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let artifacts = tempfile::tempdir().unwrap();
    let hook = common::hook_wat::create_hook_zip(
        artifacts.path(),
        "session-end-spinner",
        "on-session-end",
        "none",
        &common::hook_wat::spin_hook_wasm("on-session-end"),
    );
    common::publish_local(&home, &hook).success();
    let manifest = fs::read_to_string(&manifest_path)
        .unwrap()
        .replace(
            "capabilities:\n",
            "  - name: session-end-spinner\n    version: 0.1.0\n    runtime: hook\ncapabilities:\n  limits:\n    deadline_seconds: 5\n",
        );
    fs::write(&manifest_path, manifest).unwrap();

    let staged = stage_agent(&home, &manifest_path, Some(queue_exit_lifecycle()), None);
    fs::write(staged.accessible_workdir.join("task.md"), "Say done.").unwrap();
    let trace_path = staged.workdir.join("trace.jsonl");

    let (handle, capsule_url) = launch_in_background(staged);
    arrived
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the task.md task reached the provider");
    let (task_b, state) = send_task(&capsule_url, "m-b", "queued behind the launch task");
    assert_eq!(state, "submitted");
    release.send(()).unwrap();

    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let got = http_post_json(
            &capsule_url,
            "/",
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": task_b}})
                .to_string(),
        );
        let state = got["result"]["status"]["state"]
            .as_str()
            .unwrap_or_else(|| panic!("tasks/get answered no state: {got}"))
            .to_string();
        let done = state == "rejected";
        seen.push(state);
        if done {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "tasks/get never read rejected: {seen:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let (last, before) = seen.split_last().unwrap();
    assert_eq!(last, "rejected");
    assert!(
        before.iter().all(|state| state == "submitted"),
        "B read only submitted before rejected: {seen:?}"
    );

    let (late_task, late_state) = send_task(&capsule_url, "m-late", "after the close");
    assert_eq!(late_state, "rejected", "a closed door refuses new work");

    let canceled = http_post_json(
        &capsule_url,
        "/",
        &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/cancel", "params": {"id": task_b}})
            .to_string(),
    );
    assert!(canceled.get("error").is_none(), "{canceled}");
    assert_eq!(canceled["result"]["id"], task_b.as_str(), "{canceled}");
    assert_eq!(
        canceled["result"]["status"]["state"], "rejected",
        "an ended task is returned unchanged: {canceled}"
    );
    assert!(
        canceled["result"].get("artifacts").is_none(),
        "no residue on an ended task: {canceled}"
    );

    // The trapped hook is reported as a hook fault; the launch result is not this test's subject.
    let _ = handle.join().expect("launch thread should not panic");

    let events = read_trace(&trace_path);
    assert_sole_rejection(&events, &task_b, "session_ended");
    assert!(
        events_naming(&events, &late_task).is_empty(),
        "a door refusal is not recorded"
    );
    assert_eq!(events_named(&events, "task_rejected").len(), 1);
}

/// Under the default lifecycle (`single` + `exit`) the door takes one task while the `task.md`
/// task runs and refuses the next as busy. The one it took is refused when the session closes.
#[test]
fn lifecycle_single_exit_rejects_the_task_it_accepted_during_its_task_md_task() {
    let (server, arrived, release) = held_provider(end_turn_response("msg_1", "done"));
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path, None, None);
    fs::write(staged.accessible_workdir.join("task.md"), "Say done.").unwrap();
    let trace_path = staged.workdir.join("trace.jsonl");

    let (handle, capsule_url) = launch_in_background(staged);
    arrived
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the task.md task reached the provider");
    let (task_b, state) = send_task(&capsule_url, "m-b", "taken while the launch task runs");
    assert_eq!(state, "submitted");
    let (busy_task, busy_state) = send_task(&capsule_url, "m-c", "one too many");
    assert_eq!(busy_state, "rejected", "a single capsule holds one task");
    release.send(()).unwrap();

    let result = handle.join().expect("launch thread should not panic");
    assert!(result.is_ok(), "the launch task completed: {result:?}");

    let events = read_trace(&trace_path);
    assert_sole_rejection(&events, &task_b, "session_ended");
    assert!(
        events_naming(&events, &busy_task).is_empty(),
        "a busy refusal is not recorded"
    );
    assert_eq!(events_named(&events, "task_rejected").len(), 1);
    assert_eq!(server.requests().len(), 1);
    assert_eq!(events_named(&events, "session_end")[0]["exit_status"], "ok");
}

/// A launch task that fails still fails the launch, and a task queued behind it is still refused
/// rather than run or folded into that failure.
#[test]
fn lifecycle_a_failed_exit_task_still_rejects_what_queued_behind_it() {
    let (server, arrived, release) = held_provider(CHAT_COMPLETIONS_BODY.to_string());
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path, Some(queue_exit_lifecycle()), None);
    fs::write(staged.accessible_workdir.join("task.md"), "Say done.").unwrap();
    let trace_path = staged.workdir.join("trace.jsonl");

    let (handle, capsule_url) = launch_in_background(staged);
    arrived
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the task.md task reached the provider");
    let (task_b, state) = send_task(&capsule_url, "m-b", "queued behind a failing task");
    assert_eq!(state, "submitted");
    release.send(()).unwrap();

    let result = handle.join().expect("launch thread should not panic");
    match result {
        Err(capsule_runtime::RuntimeError::TaskDidNotComplete { exit_status, .. }) => {
            assert_eq!(exit_status, "failed");
        }
        other => panic!("the failed launch task fails the launch: {other:?}"),
    }

    let events = read_trace(&trace_path);
    assert_eq!(
        events_named(&events, "session_end")[0]["exit_status"],
        "failed"
    );
    let failures = events_named(&events, "task_failed");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_ne!(failures[0]["task_id"], task_b.as_str());
    assert_eq!(
        failures[0]["task_id"],
        events_named(&events, "task_start")[0]["task_id"]
    );
    assert_sole_rejection(&events, &task_b, "session_ended");
    assert_eq!(events_named(&events, "task_rejected").len(), 1);
}
