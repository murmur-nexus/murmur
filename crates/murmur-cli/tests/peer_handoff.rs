//! End-to-end coverage of peer file handoff: two live capsules, a real listener on each, real
//! files on disk, and the local artifact store.
//!
//! The shape every multi-capsule test here needs, and why it is built the way it is: minting
//! requires the *peer* to be reachable, because the audience is read off the peer's own agent
//! card — so the fetching capsule has to be running before the minting capsule mints. And the
//! fetching capsule's tool call has to name a handle that does not exist until the mint has
//! happened. Two things follow. Ports are pinned up front through `StageRequest::internal_port`,
//! so each capsule's manifest can name the other before either is running; and the scripted
//! inference endpoint is a *queue* a test pushes into as the run proceeds, rather than a fixed
//! list settled before launch.
//!
//! Every capsule runs `queue` + `sleep` so it outlives its tasks and can be driven a turn at a
//! time.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The shape a handle must have on the wire, asserted rather than described.
const HANDLE_PATTERN_PREFIX: &str = "mh1.";

// ── Raw HTTP ──────────────────────────────────────────────────────────────────

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

/// A raw request with no client-side path handling of any kind: what the *runtime* does with a
/// handle in a path is most of what these tests are about, so nothing here may normalise one.
fn http_request(addr: &str, method: &str, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("should connect to the capsule listener");
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response should have a header/body separator");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();

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

    HttpResponse {
        status,
        headers,
        body,
    }
}

/// A redeem with the audience asserted, which is the only form a redeem legally takes.
fn redeem(addr: &str, handle: &str, audience: &str) -> HttpResponse {
    http_request(
        addr,
        "GET",
        &format!("/resources/peer/{handle}"),
        &[("x-murmur-audience", audience)],
    )
}

fn http_post_json(addr: &str, body: &str) -> Value {
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
    serde_json::from_str(body).unwrap_or_else(|_| json!({"_raw": body}))
}

// ── A scripted endpoint that can be driven a turn at a time ───────────────────

/// A stand-in inference endpoint whose responses are a queue the test pushes into while the
/// capsule runs.
///
/// `common::ScriptedServer` settles its whole script before the listener binds, which cannot work
/// here: the fetching capsule's `fetch-peer-file` call names a handle that does not exist until
/// the *other* capsule has minted it, which needs this capsule already running.
struct QueuedServer {
    endpoint: String,
    responses: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl QueuedServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());

        let responses = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));

        let (queue, seen) = (Arc::clone(&responses), Arc::clone(&requests));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let body = read_http_body(&mut stream);
                seen.lock().unwrap().push(
                    serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({"_raw": body})),
                );

                // Wait for the test to say what this turn answers. A turn the test has not
                // scripted is a turn it did not expect, and blocking here surfaces that as the
                // test's own timeout rather than as a confusing driver error.
                let deadline = Instant::now() + Duration::from_secs(60);
                let response = loop {
                    if let Some(next) = queue.lock().unwrap().pop_front() {
                        break next;
                    }
                    if Instant::now() > deadline {
                        break end_turn_response("unscripted turn");
                    }
                    thread::sleep(Duration::from_millis(20));
                };

                let raw = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                let _ = stream.write_all(raw.as_bytes());
                let _ = stream.flush();
            }
        });

        Self {
            endpoint,
            responses,
            requests,
        }
    }

    fn push(&self, response: String) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// Every tool name the model was offered, across every request this endpoint received.
    fn offered_tools(&self) -> HashSet<String> {
        let mut names = HashSet::new();
        for request in self.requests() {
            if let Some(tools) = request.get("tools").and_then(Value::as_array) {
                for tool in tools {
                    if let Some(name) = tool.get("name").and_then(Value::as_str) {
                        names.insert(name.to_string());
                    }
                }
            }
        }
        names
    }
}

fn read_http_body(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return String::new(),
            Ok(read) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break index;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim().eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        body.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&body).to_string()
}

fn tool_use_response(tool_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": "msg_tool",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn_response(text: &str) -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// The text of the tool result the runtime fed back for `tool_id`, with the untrusted fence
/// stripped — so for both peer tools this is the JSON object they returned.
///
/// Every tool result reaches the model inside the fence, the peer tools included; these tests
/// are about what the peer plane answered, so the markers are checked and removed in one place
/// here rather than at each caller. The fence itself is covered in `untrusted_fence.rs`.
fn tool_result_text(requests: &[Value], tool_id: &str) -> Option<String> {
    for request in requests {
        for message in request.get("messages")?.as_array()? {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result")
                    || block.get("tool_use_id").and_then(Value::as_str) != Some(tool_id)
                {
                    continue;
                }
                let content = block.get("content")?;
                if let Some(text) = content.as_str() {
                    return Some(unfence(text));
                }
                if let Some(items) = content.as_array() {
                    for item in items {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            return Some(unfence(text));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Return what the fence wrapped, for a result that carries one.
///
/// A tool that ran carries a fence. A dispatch that never reached a tool — a refused handle, a
/// peer outside `capabilities.peer_fetch.allow` — comes back as the runtime's own failure text
/// and carries none, so that shape is passed through unchanged.
fn unfence(text: &str) -> String {
    let Some((open, rest)) = text.split_once('\n') else {
        return text.to_string();
    };
    if !open.starts_with("<untrusted-content source=tool:") || !open.ends_with('>') {
        return text.to_string();
    }
    rest.strip_suffix("\n</untrusted-content>")
        .unwrap_or_else(|| {
            panic!("a fenced tool result must end at the closing marker; got:\n{text}")
        })
        .to_string()
}

// ── Capsules ──────────────────────────────────────────────────────────────────

struct Capsule {
    project: PathBuf,
    session_dir: PathBuf,
    url: String,
    name: String,
    port: u16,
    server: QueuedServer,
    handle: Option<thread::JoinHandle<capsule_runtime::LaunchResult>>,
    _home: tempfile::TempDir,
}

impl Capsule {
    fn trace(&self) -> String {
        fs::read_to_string(self.session_dir.join("trace.jsonl")).unwrap_or_default()
    }

    fn events(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|error| panic!("trace line is not JSON ({error}): {line}"))
            })
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// This capsule's own audience, derived exactly as both sides derive it.
    fn audience(&self) -> String {
        format!("{}@localhost:{}", self.name, self.port).to_lowercase()
    }

    fn write_file(&self, relative: &str, bytes: &[u8]) {
        let path = self.project.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// Submits a task and waits for it to reach a terminal state.
    fn run_task(&self, id: &str, text: &str) {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {"message": {"messageId": id, "role": "user", "parts": [{"text": text}]}}
        })
        .to_string();
        let response = http_post_json(&self.url, &body);
        let task_id = response["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a task id; got: {response}"))
            .to_string();

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let body = json!({
                "jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}
            })
            .to_string();
            let response = http_post_json(&self.url, &body);
            let state = response["result"]["status"]["state"].as_str().unwrap_or("");
            if state == "completed" || state == "failed" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for task {task_id}; last response: {response}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        // A `queue` + `sleep` capsule waits indefinitely for the next task, so the launch thread
        // is abandoned rather than joined — the same shape `lifecycle.rs` uses.
        drop(self.handle.take());
    }
}

struct CapsuleSpec<'a> {
    name: &'a str,
    port: u16,
    /// Extra YAML nested inside `capabilities:` — the `peer_fetch` block, when a test declares
    /// one. Indented two spaces, because that is where it belongs.
    capabilities_yaml: &'a str,
    /// Extra top-level YAML — the `exports:` block, when a test declares one.
    exports_yaml: &'a str,
    /// Destinations added to `capabilities.network.allow` beside the inference endpoint.
    network_allow: &'a [String],
}

fn launch(spec: CapsuleSpec<'_>) -> Capsule {
    let server = QueuedServer::start();
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

    let mut network_allow = format!("      - {}\n", server.endpoint);
    for entry in spec.network_allow {
        network_allow.push_str(&format!("      - {entry}\n"));
    }
    let manifest = format!(
        "name: {name}\nversion: 0.1.0\n\
         artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
         network:\n  internal_port: {port}\n\
         capabilities:\n  network:\n    allow:\n{network_allow}{capabilities}\
         inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
         api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n{exports}",
        name = spec.name,
        port = spec.port,
        endpoint = server.endpoint,
        capabilities = spec.capabilities_yaml,
        exports = spec.exports_yaml,
    );

    let manifest_path = project.join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let staged = stage_session(
        Arc::new(LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        )),
        stage_request(&project, &runtime_manifest, spec.port),
    )
    .expect("staging should succeed");

    let session_dir = staged.workdir.clone();
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
        project,
        session_dir,
        url,
        name: spec.name.to_string(),
        port: spec.port,
        server,
        handle: Some(handle),
        _home: home,
    }
}

fn stage_request(
    project: &Path,
    runtime_manifest: &murmur_artifact::RuntimeManifest,
    port: u16,
) -> StageRequest {
    StageRequest {
        manifest_dir: project.to_path_buf(),
        capsule_name: runtime_manifest.name.clone(),
        capsule_version: runtime_manifest.version.clone(),
        capsule_component_bytes: Vec::new(),
        artifacts: runtime_manifest
            .artifacts
            .iter()
            .map(|artifact| ArtifactRequest {
                name: artifact.name.clone(),
                version: artifact.version.clone(),
                runtime: artifact.runtime.clone(),
                source: artifact.source.clone(),
                on_overflow: artifact.on_overflow,
                config: artifact.config.clone(),
                capabilities: artifact.capabilities.clone(),
            })
            .collect(),
        allowlisted_tools: HashSet::new(),
        lock_expectations: None,
        capability_policy: capability_policy_from_runtime_manifest(runtime_manifest),
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
            queue_depth: 4,
            input_timeout_secs: None,
            ..Default::default()
        }),
        lifecycle_override: None,
        trace: None,
        workdir: Some(project.to_path_buf()),
        bind_addr: "127.0.0.1".to_string(),
        internal_port: Some(port),
        declared_containment_floor: ContainmentClass::Advisory,
        exports: runtime_manifest.exports.clone(),
    }
}

/// The `exports.peer_files` block, with a `max_ttl` short enough for a `sleep` capsule — which
/// every capsule here is.
fn peer_files_yaml(root: &str, max_ttl: &str) -> String {
    format!("exports:\n  peer_files:\n    root: {root}\n    max_ttl: {max_ttl}\n")
}

fn peer_fetch_yaml(allow: &str) -> String {
    format!("  peer_fetch:\n    allow:\n      - {allow}\n")
}

/// Drives one `share-file` turn and returns the tool's parsed `data`.
fn share_file(minter: &Capsule, path: &str, peer: &str) -> Value {
    let before = minter.server.requests().len();
    minter.server.push(tool_use_response(
        "toolu_share",
        "share-file",
        json!({"path": path, "peer": peer}),
    ));
    minter.server.push(end_turn_response("shared"));
    minter.run_task(&format!("msg-share-{before}"), "share the report");

    let text = tool_result_text(&minter.server.requests(), "toolu_share").unwrap_or_else(|| {
        panic!(
            "no tool_result for share-file; requests: {:?}",
            minter.server.requests()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|_| json!({"_raw": text}))
}

/// Drives one `fetch-peer-file` turn and returns the tool's result text verbatim, so a caller can
/// assert on a failure message as easily as on a JSON result.
fn fetch_peer_file(fetcher: &Capsule, peer: &str, handle: &str, tool_id: &str) -> String {
    let before = fetcher.server.requests().len();
    fetcher.server.push(tool_use_response(
        tool_id,
        "fetch-peer-file",
        json!({"peer": peer, "handle": handle}),
    ));
    fetcher.server.push(end_turn_response("fetched"));
    fetcher.run_task(&format!("msg-fetch-{before}"), "fetch the report");

    tool_result_text(&fetcher.server.requests(), tool_id).unwrap_or_else(|| {
        panic!(
            "no tool_result for {tool_id}; requests: {:?}",
            fetcher.server.requests()
        )
    })
}

fn assert_is_a_handle(handle: &str) {
    assert!(
        handle.starts_with(HANDLE_PATTERN_PREFIX),
        "a handle must be an mh1 token; got: {handle}"
    );
    let segments: Vec<&str> = handle.split('.').collect();
    assert_eq!(segments.len(), 3, "got: {handle}");
    for segment in &segments[1..] {
        assert!(
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "segment '{segment}' must be base64url with no padding"
        );
    }
}

// ── Scenario 1, 12, 13: a file crosses, addressed only by handle ──────────────

/// A distinctive marker, so scenario 13 can prove the bytes never entered either model's context.
const MARKER: &str = "MARKER-ZQ7X-PEER-PAYLOAD";

#[test]
fn a_file_crosses_between_two_live_capsules_addressed_only_by_handle() {
    if common::skip_without_host_support(
        "a_file_crosses_between_two_live_capsules_addressed_only_by_handle",
    ) {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());

    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });

    let contents = format!("# report\n\n{MARKER}\n").into_bytes();
    minter.write_file("out/report.md", &contents);
    let expected_sha = murmur_artifact::sha256_hex(&contents);

    // ── Mint ─────────────────────────────────────────────────────────────────
    let minted = share_file(&minter, "report.md", &format!("localhost:{port_a}"));
    let handle = minted["handle"]
        .as_str()
        .unwrap_or_else(|| panic!("share-file returned no handle: {minted}"))
        .to_string();
    assert_is_a_handle(&handle);
    assert_eq!(minted["audience"], fetcher.audience());
    assert_eq!(minted["handle_id"].as_str().unwrap().len(), 16);
    assert!(minted["expires_at_ms"].as_u64().unwrap() > 0);

    // No field of the result is a filesystem path — not the workdir-relative form and not the
    // export-relative one. The handle is not an address.
    let rendered = minted.to_string();
    for forbidden in [
        "report.md",
        "out/",
        minter.project.to_str().unwrap(),
        MARKER,
    ] {
        assert!(
            !rendered.contains(forbidden),
            "share-file must not return '{forbidden}': {rendered}"
        );
    }

    // ── Fetch ────────────────────────────────────────────────────────────────
    let raw = fetch_peer_file(
        &fetcher,
        &format!("localhost:{port_b}"),
        &handle,
        "toolu_fetch",
    );
    let fetched: Value = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("fetch-peer-file should return JSON ({error}): {raw}"));

    let stored = fetched["path"].as_str().expect("a stored path");
    assert!(
        stored.starts_with("peer-in/"),
        "the runtime chooses where fetched bytes land; got {stored}"
    );
    assert_eq!(fetched["bytes"].as_u64().unwrap(), contents.len() as u64);
    assert_eq!(fetched["sha256"], expected_sha);
    assert_eq!(fetched["peer"], format!("localhost:{port_b}"));
    assert!(fetched["generation"].is_u64());

    let landed = fs::read(fetcher.project.join(stored)).expect("the fetched file should exist");
    assert_eq!(landed, contents, "the bytes must be byte-identical");

    // ── Scenario 13: bytes arrive as a file, never as context ────────────────
    assert!(
        !fetched.to_string().contains(MARKER),
        "no field of the result may hold the file's contents: {fetched}"
    );
    for request in fetcher.server.requests() {
        assert!(
            !request.to_string().contains(MARKER),
            "the fetched bytes must never enter the model's context"
        );
    }

    // ── Traces, both sides ───────────────────────────────────────────────────
    let mints = minter.events("peer_handle_mint");
    let redeems = minter.events("peer_handle_redeem");
    let fetches = fetcher.events("peer_file_fetch");
    assert_eq!(mints.len(), 1, "minter trace: {}", minter.trace());
    assert_eq!(redeems.len(), 1, "minter trace: {}", minter.trace());
    assert_eq!(fetches.len(), 1, "fetcher trace: {}", fetcher.trace());

    assert_eq!(mints[0]["outcome"], "ok");
    assert_eq!(mints[0]["path"], "report.md");
    assert_eq!(mints[0]["audience"], fetcher.audience());
    assert_eq!(redeems[0]["outcome"], "ok");
    assert_eq!(redeems[0]["path"], "report.md");
    assert_eq!(redeems[0]["audience_asserted"], fetcher.audience());
    assert_eq!(redeems[0]["sha256"], expected_sha);
    assert!(redeems[0]["generation"].is_u64());
    assert_eq!(fetches[0]["outcome"], "ok");
    assert_eq!(fetches[0]["stored_path"], stored);

    let handle_id = mints[0]["handle_id"].as_str().unwrap();
    assert_eq!(redeems[0]["handle_id"], handle_id);
    assert_eq!(fetches[0]["handle_id"], handle_id);

    // ── Scenario 12: the credential is never in the log ──────────────────────
    for (label, capsule) in [("minter", &minter), ("fetcher", &fetcher)] {
        let trace = capsule.trace();
        assert!(
            !trace.contains(HANDLE_PATTERN_PREFIX),
            "{label}: the token must never appear in a trace"
        );
        for line in trace.lines().filter(|line| !line.trim().is_empty()) {
            serde_json::from_str::<Value>(line).unwrap_or_else(|error| {
                panic!("{label}: unparseable trace line ({error}): {line}")
            });
        }
    }
    for event in mints.iter().chain(&redeems).chain(&fetches) {
        let id = event["handle_id"].as_str().expect("every event has an id");
        assert_eq!(id.len(), 16, "{event}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "{event}"
        );
    }
}

// ── Scenario 2: idempotence, tampering, wrong audience ───────────────────────

#[test]
fn the_handle_is_not_a_path_and_an_edited_handle_buys_nothing() {
    if common::skip_without_host_support(
        "the_handle_is_not_a_path_and_an_edited_handle_buys_nothing",
    ) {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });

    let contents = b"idempotent bytes".to_vec();
    minter.write_file("out/report.md", &contents);
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();
    let audience = fetcher.audience();

    // Five redeems, identical every time: no used-set, no per-handle state.
    let first = redeem(&minter.url, &handle, &audience);
    assert_eq!(first.status, 200);
    let etag = first.header("etag").unwrap().to_string();
    for _ in 0..4 {
        let again = redeem(&minter.url, &handle, &audience);
        assert_eq!(again.status, 200);
        assert_eq!(again.body, first.body);
        assert_eq!(again.header("etag").unwrap(), etag);
    }

    // Only a segment's last character carries padding bits, and those must be zero to
    // decode. Tampering with the first character keeps every substitution canonical, so
    // the segment still decodes and the refusal comes from the MAC rather than the parse.
    let flip_first = |text: &str| {
        let mut chars: Vec<char> = text.chars().collect();
        chars[0] = if chars[0] == 'A' { 'B' } else { 'A' };
        chars.into_iter().collect::<String>()
    };
    let segments: Vec<&str> = handle.split('.').collect();
    let refusals = [
        redeem(
            &minter.url,
            &format!("mh1.{}.{}", flip_first(segments[1]), segments[2]),
            &audience,
        ),
        redeem(
            &minter.url,
            &format!("mh1.{}.{}", segments[1], flip_first(segments[2])),
            &audience,
        ),
        redeem(&minter.url, &handle, "attacker@localhost:1"),
    ];
    for refusal in &refusals {
        assert_eq!(refusal.status, 403);
        assert_eq!(
            refusal.body, refusals[0].body,
            "every cause must produce a byte-identical body"
        );
        assert_eq!(refusal.json()["error"], "handle_not_valid");
        assert!(!String::from_utf8_lossy(&refusal.body).contains("idempotent bytes"));
    }

    let redeems = minter.events("peer_handle_redeem");
    let refused: Vec<&Value> = redeems
        .iter()
        .filter(|event| event["outcome"] == "handle_not_valid")
        .collect();
    assert_eq!(refused.len(), 3, "minter trace: {}", minter.trace());
    for event in refused {
        // A payload that failed the MAC is caller-controlled; it must not enter this capsule's
        // own record as if it were fact.
        assert_eq!(event["path"], Value::Null, "{event}");
        assert_eq!(event["bytes"], Value::Null);
        assert_eq!(event["sha256"], Value::Null);
    }
    assert_eq!(
        redeems
            .iter()
            .filter(|event| event["outcome"] == "ok")
            .count(),
        5
    );
}

// ── Scenario 3: no exports.peer_files means no minting at all ────────────────

#[test]
fn without_exports_peer_files_nothing_mints_and_the_plane_denies() {
    if common::skip_without_host_support(
        "without_exports_peer_files_nothing_mints_and_the_plane_denies",
    ) {
        return;
    }
    let port = common::free_port();
    let capsule = launch(CapsuleSpec {
        name: "producer",
        port,
        capabilities_yaml: "",
        exports_yaml: "",
        network_allow: &[],
    });
    capsule.write_file("out/report.md", b"never shared");

    assert!(
        !capsule
            .session_dir
            .join("tools")
            .join("share-file")
            .exists(),
        "no share-file tool directory may exist"
    );

    // One turn, so the inventory the model is offered is actually captured.
    capsule.server.push(end_turn_response("nothing to do"));
    capsule.run_task("msg-1", "hello");
    let offered = capsule.server.offered_tools();
    assert!(
        !offered.contains("share-file"),
        "the model must never see share-file: {offered:?}"
    );

    let response = redeem(&capsule.url, "mh1.YWJj.YWJj", "someone@localhost:1");
    assert_eq!(response.status, 404);
    assert_eq!(response.json()["error"], "no_peer_plane");

    // The refusal is still recorded: an undeclared capsule is exactly the one whose operator wants
    // to know somebody tried.
    let redeems = capsule.events("peer_handle_redeem");
    assert_eq!(redeems.len(), 1, "trace: {}", capsule.trace());
    assert_eq!(redeems[0]["outcome"], "no_peer_plane");
    assert_eq!(redeems[0]["path"], Value::Null);
    assert_eq!(redeems[0]["bytes"], Value::Null);
    assert!(
        capsule.events("peer_handle_mint").is_empty(),
        "trace: {}",
        capsule.trace()
    );
}

// ── Scenario 4: no capabilities.peer_fetch means no fetching ─────────────────

#[test]
fn without_peer_fetch_nothing_fetches_and_the_check_precedes_the_connection() {
    if common::skip_without_host_support(
        "without_peer_fetch_nothing_fetches_and_the_check_precedes_the_connection",
    ) {
        return;
    }
    let port = common::free_port();
    let undeclared = launch(CapsuleSpec {
        name: "reporter",
        port,
        capabilities_yaml: "",
        exports_yaml: "",
        network_allow: &[],
    });
    assert!(
        !undeclared
            .session_dir
            .join("tools")
            .join("fetch-peer-file")
            .exists(),
        "no fetch-peer-file tool directory may exist"
    );
    undeclared.server.push(end_turn_response("nothing to do"));
    undeclared.run_task("msg-1", "hello");
    assert!(!undeclared
        .server
        .offered_tools()
        .contains("fetch-peer-file"));
    drop(undeclared);

    // A capsule that declares `peer_fetch` for somewhere else, calling the real address.
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml("localhost:9"),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });
    minter.write_file("out/report.md", b"never fetched");
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();

    // Every connection the minter has accepted so far — the mint's own agent-card fetch included.
    let connections_before = minter.events("peer_handle_redeem").len();

    let result = fetch_peer_file(
        &fetcher,
        &format!("localhost:{port_b}"),
        &handle,
        "toolu_refused",
    );
    assert!(
        result.contains("capabilities.peer_fetch.allow"),
        "the refusal must name the authoriser; got: {result}"
    );

    assert_eq!(
        minter.events("peer_handle_redeem").len(),
        connections_before,
        "the check must precede the connection: the peer saw no redeem"
    );

    let fetches = fetcher.events("peer_file_fetch");
    assert_eq!(fetches.len(), 1, "fetcher trace: {}", fetcher.trace());
    assert_eq!(fetches[0]["outcome"], "peer_not_allowed");
    assert_eq!(fetches[0]["bytes"], Value::Null);
    assert_eq!(fetches[0]["stored_path"], Value::Null);
}

// ── Scenario 5: the agent cannot mint outside the declared subtree ───────────

const SECRET: &str = "SECRET-MARKER-PEER-42";

#[test]
fn the_agent_cannot_mint_outside_the_declared_subtree() {
    if common::skip_without_host_support("the_agent_cannot_mint_outside_the_declared_subtree") {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    // Two authorisers, two subtrees: `files` over `out/`, `peer_files` over `out/handoff/`.
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: "exports:\n  files:\n    root: out/\n    mode: read-only\n  \
                       peer_files:\n    root: out/handoff/\n    max_ttl: 15m\n",
        network_allow: &[format!("localhost:{port_a}")],
    });

    minter.write_file("out/handoff/ok.md", b"shareable");
    minter.write_file("out/secret.md", SECRET.as_bytes());
    minter.write_file("secret-at-top.md", SECRET.as_bytes());
    std::os::unix::fs::symlink("../secret.md", minter.project.join("out/handoff/escape.md"))
        .unwrap();

    let peer = format!("localhost:{port_a}");
    let attempts = [
        "../secret.md",
        "%2e%2e%2fsecret.md",
        "/etc/passwd",
        "escape.md",
        "handoff/../../secret-at-top.md",
    ];
    for (index, attempt) in attempts.iter().enumerate() {
        let id = format!("toolu_escape_{index}");
        minter.server.push(tool_use_response(
            &id,
            "share-file",
            json!({"path": attempt, "peer": peer}),
        ));
        minter.server.push(end_turn_response("refused"));
        minter.run_task(&format!("msg-escape-{index}"), "share it");

        let text = tool_result_text(&minter.server.requests(), &id)
            .unwrap_or_else(|| panic!("no tool_result for {attempt}"));
        assert!(
            text.contains("exports.peer_files.root"),
            "'{attempt}' must be refused naming the authoriser; got: {text}"
        );
        assert!(
            !text.contains(SECRET),
            "'{attempt}' leaked the secret: {text}"
        );
        assert!(
            !text.contains(minter.project.to_str().unwrap()),
            "'{attempt}' leaked a host path: {text}"
        );
        assert!(
            !text.contains(HANDLE_PATTERN_PREFIX),
            "'{attempt}' must return no handle: {text}"
        );
    }

    let mints = minter.events("peer_handle_mint");
    let refused: Vec<&Value> = mints
        .iter()
        .filter(|event| event["outcome"] != "ok")
        .collect();
    assert_eq!(refused.len(), attempts.len(), "trace: {}", minter.trace());
    for event in refused {
        assert_eq!(event["handle_id"], Value::Null, "{event}");
        assert_eq!(event["expires_at_ms"], Value::Null);
    }
    assert!(!minter.trace().contains(SECRET));

    // A file that *is* under the root mints and redeems.
    let handle = share_file(&minter, "ok.md", &peer)["handle"]
        .as_str()
        .unwrap()
        .to_string();
    let served = redeem(&minter.url, &handle, &fetcher.audience());
    assert_eq!(served.status, 200);
    assert_eq!(served.body, b"shareable".to_vec());

    // The operator plane is a separate authoriser over a separate subtree, and is unaffected.
    let operator = http_request(&minter.url, "GET", "/resources/files/secret.md", &[]);
    assert_eq!(operator.status, 200, "body: {:?}", operator.json());
    assert_eq!(operator.body, SECRET.as_bytes().to_vec());
}

// ── Scenario 6: a third capsule holding the handle cannot redeem it ──────────

#[test]
fn a_third_capsule_holding_the_handle_cannot_redeem_it() {
    if common::skip_without_host_support("a_third_capsule_holding_the_handle_cannot_redeem_it") {
        return;
    }
    let (port_a, port_b, port_c) = (
        common::free_port(),
        common::free_port(),
        common::free_port(),
    );
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });
    let third = launch(CapsuleSpec {
        name: "interloper",
        port: port_c,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });

    minter.write_file("out/report.md", SECRET.as_bytes());
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();

    // C holds the handle and is allowed to talk to B — and still cannot redeem, because the
    // handle was not minted for C's identity.
    let via_tool = fetch_peer_file(
        &third,
        &format!("localhost:{port_b}"),
        &handle,
        "toolu_third",
    );
    assert!(
        via_tool.contains("handle_not_valid"),
        "C's tool call must fail; got: {via_tool}"
    );
    assert!(!via_tool.contains(SECRET));

    let as_c = redeem(&minter.url, &handle, &third.audience());
    assert_eq!(as_c.status, 403);
    assert_eq!(as_c.json()["error"], "handle_not_valid");

    let tampered = redeem(&minter.url, &format!("{handle}A"), &fetcher.audience());
    assert_eq!(as_c.body, tampered.body, "same body as any other tamper");

    let no_header = http_request(
        &minter.url,
        "GET",
        &format!("/resources/peer/{handle}"),
        &[],
    );
    assert_eq!(no_header.status, 400);
    assert_eq!(no_header.json()["error"], "missing_audience");

    for response in [&as_c, &tampered, &no_header] {
        assert!(
            !String::from_utf8_lossy(&response.body).contains(SECRET),
            "no refusal may serve any of the file's bytes"
        );
    }
}

// ── Scenario 7: a rewritten file redeems to its current bytes ────────────────

#[test]
fn a_rewritten_file_redeems_to_its_current_bytes_and_the_etag_says_so() {
    if common::skip_without_host_support(
        "a_rewritten_file_redeems_to_its_current_bytes_and_the_etag_says_so",
    ) {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });

    let first = b"first contents".to_vec();
    minter.write_file("out/report.md", &first);
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();
    let audience = fetcher.audience();

    let one = redeem(&minter.url, &handle, &audience);
    assert_eq!(one.status, 200);
    assert_eq!(one.body, first);
    let etag_one = one.header("etag").unwrap().to_string();
    let generation_one: u64 = one.header("x-murmur-generation").unwrap().parse().unwrap();
    assert_eq!(
        etag_one,
        format!("\"sha256:{}\"", murmur_artifact::sha256_hex(&first))
    );

    // A second completed task, so the generation moves.
    minter.server.push(end_turn_response("done"));
    minter.run_task("msg-second-task", "do something");

    // Rewritten by rename-into-place, the convention the reader's atomicity rests on.
    let second = b"second contents, longer than the first".to_vec();
    let tmp = minter.project.join("out/report.md.tmp");
    fs::write(&tmp, &second).unwrap();
    fs::rename(&tmp, minter.project.join("out/report.md")).unwrap();

    let two = redeem(&minter.url, &handle, &audience);
    assert_eq!(two.status, 200, "the same handle must still redeem");
    assert_eq!(two.body, second, "a redeem serves the file's current bytes");
    assert_eq!(
        two.header("etag").unwrap(),
        format!("\"sha256:{}\"", murmur_artifact::sha256_hex(&second))
    );
    assert_ne!(two.header("etag").unwrap(), etag_one);
    let generation_two: u64 = two.header("x-murmur-generation").unwrap().parse().unwrap();
    assert!(
        generation_two > generation_one,
        "the generation must advance with completed turns: {generation_one} -> {generation_two}"
    );

    let redeems = minter.events("peer_handle_redeem");
    assert_eq!(redeems.len(), 2, "trace: {}", minter.trace());
    for event in &redeems {
        assert_eq!(event["outcome"], "ok", "no 409 is reachable on this plane");
    }
}

/// The other half of scenario 7, and the cheapest possible statement of it: no conflict code
/// exists anywhere in the tree, so none can be reached.
#[test]
fn no_generation_conflict_code_exists_in_the_codebase() {
    // Assembled rather than written out, so this file is not itself a hit.
    let needle = format!("generation{}moved", "_");
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ is the parent of this crate");
    let mut offenders = Vec::new();
    let mut pending = vec![crates_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && fs::read_to_string(&path).is_ok_and(|text| text.contains(&needle))
            {
                offenders.push(path);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the peer plane has no conflict verb; found `{needle}` in {offenders:?}"
    );
}

// ── Scenario 8: an expired handle is refused ─────────────────────────────────

#[test]
fn an_expired_handle_is_refused() {
    if common::skip_without_host_support("an_expired_handle_is_refused") {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "2s"),
        network_allow: &[format!("localhost:{port_a}")],
    });

    minter.write_file("out/report.md", b"briefly available");
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();
    let audience = fetcher.audience();

    assert_eq!(redeem(&minter.url, &handle, &audience).status, 200);
    thread::sleep(Duration::from_secs(3));

    let expired = redeem(&minter.url, &handle, &audience);
    assert_eq!(expired.status, 410);
    assert_eq!(expired.json()["error"], "handle_expired");
    assert!(!String::from_utf8_lossy(&expired.body).contains("briefly available"));

    let outcomes: Vec<String> = minter
        .events("peer_handle_redeem")
        .iter()
        .map(|event| event["outcome"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        outcomes,
        vec!["ok".to_string(), "handle_expired".to_string()]
    );
}

// ── Scenario 10: a handle does not survive the instance that minted it ───────

#[test]
fn a_handle_does_not_survive_the_instance_that_minted_it() {
    if common::skip_without_host_support("a_handle_does_not_survive_the_instance_that_minted_it") {
        return;
    }
    let (port_a, port_b, port_b2) = (
        common::free_port(),
        common::free_port(),
        common::free_port(),
    );
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let audience = fetcher.audience();

    let first = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });
    first.write_file("out/report.md", SECRET.as_bytes());
    let project = first.project.clone();
    let handle = share_file(&first, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(redeem(&first.url, &handle, &audience).status, 200);

    // A second session of the same capsule, over the same project directory, with the same
    // manifest and the same file still on disk. It binds a different port for one reason only:
    // session one is deliberately still running, so the two instances can be compared side by
    // side rather than in sequence. Nothing about a handle depends on the minter's port — the
    // audience is derived from the *fetcher's* identity — so this is the same question the
    // scenario asks, with both answers observable at once.
    let manifest = fs::read_to_string(project.join("murmur.yaml"))
        .unwrap()
        .replace(
            &format!("internal_port: {port_b}"),
            &format!("internal_port: {port_b2}"),
        );
    let second = relaunch(&project, &manifest, port_b2);
    assert!(project.join("out/report.md").exists());
    assert!(
        redeem(&first.url, &handle, &audience).status == 200,
        "the minting instance still serves it"
    );

    let refused = redeem(&second.url, &handle, &audience);
    assert_eq!(refused.status, 403);
    assert_eq!(refused.json()["error"], "handle_not_valid");
    assert!(!String::from_utf8_lossy(&refused.body).contains(SECRET));

    let redeems = second.events("peer_handle_redeem");
    let last = redeems.last().expect("the refusal is recorded");
    assert_eq!(last["outcome"], "handle_not_valid");
    assert_eq!(last["path"], Value::Null);
}

/// Relaunches a capsule over an existing project directory, keeping its manifest and files.
fn relaunch(project: &Path, manifest: &str, port: u16) -> Capsule {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = project.join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let staged = stage_session(
        Arc::new(LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        )),
        stage_request(project, &runtime_manifest, port),
    )
    .expect("the second session should stage");

    let session_dir = staged.workdir.clone();
    let (url_tx, url_rx) = mpsc::channel::<String>();
    let handle = thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("the second session should launch")
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("timed out waiting for the second session's URL");

    Capsule {
        project: project.to_path_buf(),
        session_dir,
        url,
        name: runtime_manifest.name.clone(),
        port,
        server: QueuedServer::start(),
        handle: Some(handle),
        _home: home,
    }
}

// ── Scenario 11: the peer plane has no listing verb ──────────────────────────

#[test]
fn the_peer_plane_has_no_listing_verb_and_no_write_path() {
    if common::skip_without_host_support("the_peer_plane_has_no_listing_verb_and_no_write_path") {
        return;
    }
    let (port_a, port_b) = (common::free_port(), common::free_port());
    let fetcher = launch(CapsuleSpec {
        name: "reporter",
        port: port_a,
        capabilities_yaml: &peer_fetch_yaml(&format!("localhost:{port_b}")),
        exports_yaml: "",
        network_allow: &[],
    });
    let minter = launch(CapsuleSpec {
        name: "producer",
        port: port_b,
        capabilities_yaml: "",
        exports_yaml: &peer_files_yaml("out/", "15m"),
        network_allow: &[format!("localhost:{port_a}")],
    });

    let contents = b"the only file".to_vec();
    minter.write_file("out/report.md", &contents);
    minter.write_file("out/other.md", b"a second file");
    let before = murmur_artifact::sha256_hex(&contents);
    let handle = share_file(&minter, "report.md", &format!("localhost:{port_a}"))["handle"]
        .as_str()
        .unwrap()
        .to_string();

    for path in ["/resources/peer", "/resources/peer/"] {
        let response = http_request(
            &minter.url,
            "GET",
            path,
            &[("x-murmur-audience", &fetcher.audience())],
        );
        assert_eq!(response.status, 404, "{path}");
        assert_eq!(response.json()["error"], "not_found", "{path}");
        let body = String::from_utf8_lossy(&response.body);
        for forbidden in ["report.md", "other.md", "entries"] {
            assert!(
                !body.contains(forbidden),
                "{path} must not name '{forbidden}': {body}"
            );
        }
    }

    let not_a_handle = http_request(
        &minter.url,
        "GET",
        "/resources/peer/report.md",
        &[("x-murmur-audience", &fetcher.audience())],
    );
    assert_eq!(not_a_handle.status, 400);
    assert_eq!(not_a_handle.json()["error"], "malformed_handle");

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = http_request(
            &minter.url,
            method,
            &format!("/resources/peer/{handle}"),
            &[("x-murmur-audience", &fetcher.audience())],
        );
        assert_eq!(response.status, 405, "{method}");
        assert_eq!(response.json()["error"], "method_not_allowed");
        assert_eq!(response.header("allow").unwrap(), "GET");
    }

    assert_eq!(
        murmur_artifact::sha256_hex(&fs::read(minter.project.join("out/report.md")).unwrap()),
        before,
        "no write verb may have changed the file"
    );
}

// ── Scenario 9, 14, 15: the CLI surface ──────────────────────────────────────

fn mur_run(home: &Path, project: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .current_dir(project)
        .arg("run")
        .arg("--manifest")
        .arg("murmur.yaml")
        .args(args)
        .assert()
}

/// A script capsule fixture: no `inference:` block, so nothing here needs a driver artifact or a
/// scripted endpoint. Every case below refuses (or reports) before a capsule would ever run.
fn write_manifest_only_project(dir: &Path, extra_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: peer-fixture\nversion: 0.0.1\n{extra_yaml}"),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn assert_no_workdir(project: &Path) {
    assert!(
        !project.join(".murmur").exists() && !project.join("workdir").exists(),
        "no workdir may be created: {:?}",
        fs::read_dir(project)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_persistent_capsule_must_declare_a_short_enough_max_ttl() {
    let refusals = [
        // Declared `sleep` with no `max_ttl` at all.
        "lifecycle:\n  after_task: sleep\nexports:\n  peer_files:\n    root: out/\n",
        // Declared `sleep` with a `max_ttl` above the ceiling.
        "lifecycle:\n  after_task: sleep\nexports:\n  peer_files:\n    root: out/\n    max_ttl: 30m\n",
    ];
    for extra in refusals {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_manifest_only_project(project.path(), extra);

        let assert = mur_run(home.path(), project.path(), &[]).failure();
        let output = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("error[E-CAP-008]"), "got: {combined}");
        assert!(
            combined.contains("exports.peer_files.max_ttl"),
            "got: {combined}"
        );
        assert!(
            combined.contains("lifecycle.after_task: sleep"),
            "got: {combined}"
        );
        assert!(combined.contains("900s"), "the ceiling: {combined}");
        assert!(
            combined.contains("relaunch") && combined.contains("request"),
            "the alternative to a longer handle: {combined}"
        );
        assert_no_workdir(project.path());
    }

    // `sleep` with a `max_ttl` under the ceiling gets past the gate. It fails later — the fixture
    // is not a valid component — which is exactly what proves the gate was not what stopped it.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_manifest_only_project(
        project.path(),
        "lifecycle:\n  after_task: sleep\nexports:\n  peer_files:\n    root: out/\n    max_ttl: 10m\n",
    );
    let assert = mur_run(home.path(), project.path(), &[]);
    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!combined.contains("E-CAP-008"), "got: {combined}");

    // `exit` with no `max_ttl` launches, and reports the ephemeral default.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_manifest_only_project(project.path(), "exports:\n  peer_files:\n    root: out/\n");
    let output = mur_run(home.path(), project.path(), &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(report["peer_files"]["max_ttl_secs"], 3600);
}

#[test]
fn declaring_peer_handoff_does_not_change_the_achieved_containment_class() {
    let blocks = "capabilities:\n  peer_fetch:\n    allow:\n      - localhost:41234\n\
                  exports:\n  peer_files:\n    root: out/\n    max_ttl: 30m\n";

    let home = tempfile::tempdir().unwrap();
    let without_dir = tempfile::tempdir().unwrap();
    write_manifest_only_project(without_dir.path(), "");
    let with_dir = tempfile::tempdir().unwrap();
    write_manifest_only_project(with_dir.path(), blocks);

    let report = |dir: &Path| -> serde_json::Value {
        let output = mur_run(home.path(), dir, &["--explain-scope", "--json"])
            .success()
            .get_output()
            .stdout
            .clone();
        let line = String::from_utf8(output).unwrap();
        assert_eq!(line.trim().lines().count(), 1, "one JSON line: {line}");
        serde_json::from_str(line.trim()).unwrap()
    };

    let without = report(without_dir.path());
    let with = report(with_dir.path());

    for key in ["achieved_containment", "enforcement_tier", "floor_met"] {
        assert_eq!(with[key], without[key], "{key} must not move");
    }
    assert_eq!(without["peer_files"], serde_json::Value::Null);
    assert_eq!(without["peer_fetch_allow"], json!([]));
    assert_eq!(
        with["peer_files"],
        json!({"root": "out/", "max_ttl_secs": 1800, "max_bytes": 10_485_760})
    );
    assert_eq!(with["peer_fetch_allow"], json!(["localhost:41234"]));

    assert_no_workdir(without_dir.path());
    assert_no_workdir(with_dir.path());
}

#[test]
fn malformed_peer_grants_are_rejected_at_manifest_parse_time() {
    let cases: [(&str, &str, &str); 7] = [
        (
            "exports:\n  peer_files:\n    root: ../out\n",
            "exports.peer_files.root",
            "must be a relative path inside the workdir",
        ),
        (
            "exports:\n  peer_files:\n    root: /etc\n",
            "exports.peer_files.root",
            "must be a relative path inside the workdir",
        ),
        (
            "exports:\n  peer_files:\n    max_ttl: 30m\n",
            "exports.peer_files.root",
            "must be a relative path inside the workdir",
        ),
        (
            "exports:\n  peer_files:\n    root: out/\n    max_ttl: \"5 minutes\"\n",
            "exports.peer_files.max_ttl",
            "must be a duration: an integer, optionally suffixed s/m/h",
        ),
        (
            "exports:\n  peer_files:\n    root: out/\n    max_bytes: 10MB\n",
            "exports.peer_files.max_bytes",
            "must be a byte count, optionally suffixed Ki/Mi/Gi",
        ),
        (
            "capabilities:\n  peer_fetch:\n    allow: []\n",
            "capabilities.peer_fetch.allow",
            "must be a non-empty list of network destinations",
        ),
        (
            "capabilities:\n  peer_fetch:\n    allow:\n      - \"http://[not a host\"\n",
            "capabilities.peer_fetch.allow",
            "must be a non-empty list of network destinations",
        ),
    ];

    for (extra, field, accepted_form) in cases {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_manifest_only_project(project.path(), extra);

        let assert = mur_run(home.path(), project.path(), &[]).failure();
        let output = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined.contains("error[E-MAN-003]"),
            "{extra:?} -> {combined}"
        );
        assert!(combined.contains(field), "{extra:?} -> {combined}");
        assert!(combined.contains(accepted_form), "{extra:?} -> {combined}");
        assert_no_workdir(project.path());
    }
}
