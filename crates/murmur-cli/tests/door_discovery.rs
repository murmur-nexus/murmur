//! A remote consumer's view of what a capsule's door answers: the agent card's `serves` block,
//! checked against the answers the same door gives over real HTTP.
//!
//! Each test launches a real capsule through `stage_session` and `launch_session`, reads its card,
//! and then asks the door every method the card lists — and one it does not — so an advertisement
//! that drifts from dispatch fails here rather than in a consumer.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

const ALL_METHODS: [&str; 6] = [
    "message/send",
    "message/stream",
    "stream/watch",
    "tasks/get",
    "tasks/cancel",
    "session/stop",
];

const EXPORTS_FILES: &str = "exports:\n  files:\n    root: out/\n    mode: read-only\n";
const EXPORTS_PEER_FILES: &str = "exports:\n  peer_files:\n    root: out/\n    max_ttl: 15m\n";
const EXPORTS_BOTH: &str = "exports:\n  files:\n    root: out/\n    mode: read-only\n  \
                            peer_files:\n    root: out/\n    max_ttl: 15m\n";

// ── HTTP ──────────────────────────────────────────────────────────────────────

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "expected a JSON body; got: {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

fn parse_head(head: &str) -> (u16, Vec<(String, String)>) {
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {status_line}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    (status, headers)
}

fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("should connect to the capsule listener");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response should have a header/body separator");
    let (status, headers) = parse_head(&String::from_utf8_lossy(&raw[..split]));
    HttpResponse {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

fn post_jsonrpc(addr: &str, body: &Value) -> Value {
    let body = body.to_string();
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).to_string();
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|_| panic!("expected a JSON-RPC body; got: {raw}"))
}

/// Posts a streaming method and returns the status and headers only. The connection is dropped
/// once the headers are read, so the body is always empty: an SSE response has no end to wait for.
fn post_stream_head(addr: &str, body: &Value) -> HttpResponse {
    let body = body.to_string();
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .expect("should read a header line");
        if read == 0 || line.trim().is_empty() {
            break;
        }
        head.push_str(&line);
    }
    let (status, headers) = parse_head(&head);
    HttpResponse {
        status,
        headers,
        body: Vec::new(),
    }
}

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params})
}

fn message_params(id: &str) -> Value {
    json!({"message": {"messageId": id, "role": "user", "parts": [{"text": "hello"}]}})
}

fn string_list(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("expected an array; got {value}"))
        .iter()
        .map(|item| {
            item.as_str()
                .unwrap_or_else(|| panic!("expected a string; got {item}"))
        })
        .collect()
}

// ── Harness ───────────────────────────────────────────────────────────────────

struct Capsule {
    url: String,
    session_id: String,
    handle: Option<thread::JoinHandle<capsule_runtime::LaunchResult>>,
    _project: PathBuf,
    _home: tempfile::TempDir,
    _server: common::ScriptedServer,
}

impl Capsule {
    fn card(&self) -> Value {
        let response = http_request(&self.url, "GET", "/.well-known/agent-card.json", &[]);
        assert_eq!(response.status, 200);
        response.json()
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        // A `sleep` capsule waits indefinitely for the next task, so the launch thread is
        // abandoned rather than joined.
        drop(self.handle.take());
    }
}

fn manifest_yaml(endpoint: &str, exports: Option<&str>) -> String {
    format!(
        "name: door-discovery-agent\nversion: 0.1.0\n\
         artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
         capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n{}",
        exports.unwrap_or("")
    )
}

/// Launches a capsule whose door stays up while it is asked every method in turn.
///
/// A `queue` or `single` capsule outlives its tasks (`after_task: sleep`). A `none` capsule takes
/// no task over the door and ends once `task.md` is done, so it is given a `task.md` whose
/// inference is held for longer than any test here runs.
fn launch(task_acceptance: TaskAcceptance, exports: Option<&str>) -> Capsule {
    let accepts_no_tasks = matches!(task_acceptance, TaskAcceptance::None);
    let inference_delay = if accepts_no_tasks {
        Duration::from_secs(120)
    } else {
        Duration::ZERO
    };
    let server = common::ScriptedServer::start_with_delay(
        (0..4)
            .map(|index| {
                json!({
                    "id": format!("msg_{index}"),
                    "type": "message",
                    "role": "assistant",
                    "model": "test-model",
                    "content": [{"type": "text", "text": "done"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })
                .to_string()
            })
            .collect(),
        inference_delay,
    );

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap().keep();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = project.join("murmur.yaml");
    fs::write(&manifest_path, manifest_yaml(&server.endpoint, exports)).unwrap();
    fs::create_dir_all(project.join("out")).unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let requested_artifacts: Vec<ArtifactRequest> = runtime_manifest
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

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
            manifest_dir: project.clone(),
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
                task_acceptance,
                after_task: AfterTask::Sleep,
                queue_depth: 4,
                input_timeout_secs: None,
                ..Default::default()
            }),
            lifecycle_override: None,
            trace: None,
            workdir: Some(project.clone()),
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: runtime_manifest.exports.clone(),
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .expect("staging should succeed");
    if accepts_no_tasks {
        fs::write(project.join("task.md"), "Hold the door open").unwrap();
    }

    let session_id = staged.session_id.clone();
    let (url_tx, url_rx) = mpsc::channel::<String>();
    let handle = thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("timed out waiting for the capsule URL");

    Capsule {
        url,
        session_id,
        handle: Some(handle),
        _project: project,
        _home: home,
        _server: server,
    }
}

// ── S1: every advertised method is answered ───────────────────────────────────

#[test]
fn every_method_the_card_lists_is_answered_and_an_unlisted_one_is_not() {
    if common::skip_without_host_support(
        "every_method_the_card_lists_is_answered_and_an_unlisted_one_is_not",
    ) {
        return;
    }
    let capsule = launch(TaskAcceptance::Queue, None);
    let card = capsule.card();

    assert_eq!(string_list(&card["serves"]["methods"]), ALL_METHODS);
    assert_eq!(card["serves"]["planes"], json!([]));
    assert_eq!(card["capabilities"]["streaming"], true);

    let sent = post_jsonrpc(&capsule.url, &rpc("message/send", message_params("m-send")));
    assert_ne!(sent["error"]["code"], -32601, "message/send: {sent}");
    let task_id = sent["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("message/send should start a task; got {sent}"))
        .to_string();

    for method in ["message/stream", "stream/watch"] {
        let params = if method == "message/stream" {
            message_params("m-stream")
        } else {
            json!({})
        };
        let head = post_stream_head(&capsule.url, &rpc(method, params));
        assert_eq!(head.status, 200, "{method}");
        assert_eq!(
            head.header("content-type"),
            Some("text/event-stream"),
            "{method}"
        );
    }

    let got = post_jsonrpc(&capsule.url, &rpc("tasks/get", json!({"id": task_id})));
    assert_ne!(got["error"]["code"], -32601, "tasks/get: {got}");
    assert_eq!(got["result"]["id"], task_id.as_str(), "tasks/get: {got}");

    let cancelled = post_jsonrpc(&capsule.url, &rpc("tasks/cancel", json!({"id": task_id})));
    assert_ne!(
        cancelled["error"]["code"], -32601,
        "tasks/cancel: {cancelled}"
    );

    // Asked before `session/stop`, while the session is certainly still answering.
    let unlisted = post_jsonrpc(&capsule.url, &rpc("tasks/list", json!({})));
    assert_eq!(unlisted["error"]["code"], -32601, "tasks/list: {unlisted}");
    assert_eq!(unlisted["error"]["message"], "Method not found");

    let stopped = post_jsonrpc(&capsule.url, &rpc("session/stop", json!({})));
    assert_ne!(stopped["error"]["code"], -32601, "session/stop: {stopped}");
    assert_eq!(stopped["result"]["session_id"], capsule.session_id.as_str());
}

// ── S2: `task_acceptance: none` lists neither task-starting method ────────────

#[test]
fn a_capsule_that_accepts_no_tasks_lists_neither_task_starting_method() {
    if common::skip_without_host_support(
        "a_capsule_that_accepts_no_tasks_lists_neither_task_starting_method",
    ) {
        return;
    }
    let capsule = launch(TaskAcceptance::None, None);
    let card = capsule.card();

    assert_eq!(
        string_list(&card["serves"]["methods"]),
        ["stream/watch", "tasks/get", "tasks/cancel", "session/stop"]
    );
    assert_eq!(card["capabilities"]["streaming"], false);

    for (method, params) in [
        ("message/send", message_params("m-send")),
        ("message/stream", message_params("m-stream")),
    ] {
        let refused = post_jsonrpc(&capsule.url, &rpc(method, params));
        assert_eq!(refused["error"]["code"], -32601, "{method}: {refused}");
        assert_eq!(refused["error"]["message"], "Method not found");
    }

    let got = post_jsonrpc(
        &capsule.url,
        &rpc("tasks/get", json!({"id": "tsk_unknown"})),
    );
    assert!(
        got["error"].is_object(),
        "an unknown task is an error: {got}"
    );
    assert_ne!(got["error"]["code"], -32601, "tasks/get: {got}");
}

// ── S3: a plane is listed exactly when it is declared, and a listed plane serves ─

#[test]
fn a_declared_files_export_is_listed_and_serves() {
    if common::skip_without_host_support("a_declared_files_export_is_listed_and_serves") {
        return;
    }
    let capsule = launch(TaskAcceptance::Queue, Some(EXPORTS_FILES));
    assert_eq!(capsule.card()["serves"]["planes"], json!(["files"]));

    let listed = http_request(&capsule.url, "GET", "/resources/files", &[]);
    assert_eq!(listed.status, 200, "body: {}", listed.json());
}

#[test]
fn a_declared_peer_files_export_is_listed_and_serves() {
    if common::skip_without_host_support("a_declared_peer_files_export_is_listed_and_serves") {
        return;
    }
    let capsule = launch(TaskAcceptance::Queue, Some(EXPORTS_PEER_FILES));
    assert_eq!(capsule.card()["serves"]["planes"], json!(["peer_files"]));

    let redeemed = http_request(
        &capsule.url,
        "GET",
        "/resources/peer/not-a-token",
        &[("x-murmur-audience", "probe@127.0.0.1:1")],
    );
    assert_eq!(redeemed.status, 400, "body: {}", redeemed.json());
    assert_eq!(redeemed.json()["error"], "malformed_handle");
}

#[test]
fn an_undeclared_plane_is_not_listed_and_only_refuses() {
    if common::skip_without_host_support("an_undeclared_plane_is_not_listed_and_only_refuses") {
        return;
    }
    let capsule = launch(TaskAcceptance::Queue, None);
    assert_eq!(capsule.card()["serves"]["planes"], json!([]));

    let listed = http_request(&capsule.url, "GET", "/resources/files", &[]);
    assert_eq!(listed.status, 404);
    assert_eq!(listed.json()["error"], "no_resource_plane");

    let redeemed = http_request(
        &capsule.url,
        "GET",
        "/resources/peer/not-a-token",
        &[("x-murmur-audience", "probe@127.0.0.1:1")],
    );
    assert_eq!(redeemed.status, 404);
    assert_eq!(redeemed.json()["error"], "no_peer_plane");
}

#[test]
fn both_declared_planes_are_listed_files_first() {
    if common::skip_without_host_support("both_declared_planes_are_listed_files_first") {
        return;
    }
    let capsule = launch(TaskAcceptance::Queue, Some(EXPORTS_BOTH));
    assert_eq!(
        capsule.card()["serves"]["planes"],
        json!(["files", "peer_files"])
    );
    assert_eq!(
        http_request(&capsule.url, "GET", "/resources/files", &[]).status,
        200
    );
}

// ── S5: the card change is additive ───────────────────────────────────────────

#[test]
fn the_card_keeps_every_existing_key_and_adds_only_serves() {
    if common::skip_without_host_support("the_card_keeps_every_existing_key_and_adds_only_serves") {
        return;
    }
    let capsule = launch(TaskAcceptance::Single, None);
    let card = capsule.card();

    let keys: HashSet<&str> = card
        .as_object()
        .expect("the card is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        HashSet::from([
            "name",
            "version",
            "url",
            "session_id",
            "capabilities",
            "serves"
        ])
    );

    let capabilities = card["capabilities"]
        .as_object()
        .expect("capabilities is an object");
    let capability_keys: HashSet<&str> = capabilities.keys().map(String::as_str).collect();
    assert_eq!(
        capability_keys,
        HashSet::from(["tools", "shell", "network", "streaming", "cancellation"])
    );

    assert_eq!(card["name"], "door-discovery-agent");
    assert_eq!(card["version"], "0.1.0");
    assert_eq!(card["url"], capsule.url.as_str());
    assert_eq!(card["session_id"], capsule.session_id.as_str());
    assert!(capabilities["tools"].is_array());
    assert_eq!(capabilities["shell"], false);
    assert_eq!(capabilities["network"], true, "the endpoint is allowlisted");
    assert_eq!(capabilities["streaming"], true);
    assert_eq!(
        capabilities["cancellation"], true,
        "an http capsule can stop a task"
    );

    let serves = card["serves"].as_object().expect("serves is an object");
    let serves_keys: HashSet<&str> = serves.keys().map(String::as_str).collect();
    assert_eq!(serves_keys, HashSet::from(["methods", "planes"]));
    assert_eq!(string_list(&serves["methods"]), ALL_METHODS);
    assert!(serves["planes"].is_array());
}
