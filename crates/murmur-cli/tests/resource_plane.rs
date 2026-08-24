//! End-to-end coverage of the capsule resource plane against a real listener, a real filesystem
//! and the local artifact store.
//!
//! Every test here stages with `StageRequest::workdir` pointed at the project directory, so the
//! session's *accessible* workdir — the one `exports.files.root` resolves against and the one the
//! agent sees at `.` — is the directory the test itself wrote files into. `trace.jsonl` lives in
//! the internal `.murmur/<session_id>` directory beside it, which no export rooted in the project
//! can reach.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, explain_scope, launch_session, stage_session,
    AfterTask, ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::Value;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

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

/// A raw request with no client-side path handling of any kind: the point of most of these tests
/// is what the *runtime* does with a path, so nothing here may normalise one on its way out.
fn http_request(addr: &str, method: &str, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(addr).expect("should connect to the capsule listener");
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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

    let response = HttpResponse {
        status,
        headers,
        body,
    };
    // Checked on every response every test makes, rather than in one test of its own: a body
    // delimited only by the connection closing is one a caller cannot tell apart from a truncated
    // one, and scenario 13 turns on never seeing a short body.
    let declared: usize = response
        .header("content-length")
        .unwrap_or_else(|| panic!("{method} {path}: every response declares a content-length"))
        .parse()
        .expect("content-length should be a number");
    assert_eq!(
        declared,
        response.body.len(),
        "{method} {path}: content-length must match the bytes actually received"
    );
    response
}

fn http_get(addr: &str, path: &str) -> HttpResponse {
    http_request(addr, "GET", path)
}

fn http_post_json(addr: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).to_string();
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({"_raw": body}))
}

fn message_send_body(id: &str, text: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": {
            "message": {"messageId": id, "role": "user", "parts": [{"text": text}]}
        }
    })
    .to_string()
}

// ── Harness ───────────────────────────────────────────────────────────────────

/// A launched capsule that stays alive after its tasks finish (`queue` + `sleep`), so the
/// resource plane can be exercised while the session is idle *and* mid-task.
struct Capsule {
    project: PathBuf,
    /// Internal session directory — where `trace.jsonl` is.
    session_dir: PathBuf,
    url: String,
    handle: Option<thread::JoinHandle<capsule_runtime::LaunchResult>>,
    _home: tempfile::TempDir,
    _server: common::ScriptedServer,
}

impl Capsule {
    fn trace(&self) -> String {
        fs::read_to_string(self.session_dir.join("trace.jsonl")).unwrap_or_default()
    }

    fn trace_events(&self, event_type: &str) -> Vec<Value> {
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

    fn submit_and_await(&self, id: &str, text: &str) {
        let response = http_post_json(&self.url, "/", &message_send_body(id, text));
        let task_id = response["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a task id; got: {response}"))
            .to_string();
        self.await_task(&task_id);
    }

    fn await_task(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}
            })
            .to_string();
            let response = http_post_json(&self.url, "/", &body);
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

/// The `exports:` block a capsule is launched with, or `None` for a capsule that declares none.
fn manifest_yaml(endpoint: &str, exports: Option<&str>) -> String {
    format!(
        "name: resource-plane-agent\nversion: 0.1.0\n\
         artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
         capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
         api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n{}",
        exports.unwrap_or("")
    )
}

fn launch(exports: Option<&str>, responses: usize) -> Capsule {
    let server = common::ScriptedServer::start(
        (0..responses.max(1))
            .map(|index| {
                serde_json::json!({
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
            capabilities: artifact.capabilities.clone(),
        })
        .collect();

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
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
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            // `queue` + `sleep`: the capsule outlives its tasks, which is the case the resource
            // plane exists for — a finished-but-alive capsule read by a gateway.
            lifecycle: Some(LifecycleConfig {
                task_acceptance: TaskAcceptance::Queue,
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
        },
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
        handle: Some(handle),
        _home: home,
        _server: server,
    }
}

const EXPORTS_OUT: &str = "exports:\n  files:\n    root: out/\n    mode: read-only\n";

fn write_export_file(project: &Path, relative: &str, bytes: &[u8]) {
    let path = project.join("out").join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The class this host actually achieves for a capsule that declares nothing special — the same
/// value `mur run --explain-scope --json` prints, through the same function.
fn achieved_class_here() -> ContainmentClass {
    explain_scope(
        &capsule_runtime::CapabilityPolicy::default(),
        ContainmentClass::Advisory,
        None,
    )
    .achieved_containment
}

// ── Scenario 1: happy path ────────────────────────────────────────────────────

#[test]
fn a_declared_export_lists_and_reads_without_buying_a_turn() {
    if common::skip_without_host_support("a_declared_export_lists_and_reads_without_buying_a_turn")
    {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    let bytes = b"# report\n\nthe agent wrote this.\n";
    write_export_file(&capsule.project, "report.md", bytes);
    let expected_sha = murmur_artifact::sha256_hex(bytes);

    let list = http_get(&capsule.url, "/resources/files");
    assert_eq!(list.status, 200, "body: {:?}", list.json());
    let listed = list.json();
    assert_eq!(listed["root"], "out/");
    assert_eq!(listed["mode"], "read-only");
    assert_eq!(listed["max_bytes"], 10_485_760);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 1);
    let entry = &listed["entries"][0];
    assert_eq!(entry["path"], "report.md");
    assert_eq!(entry["size_bytes"], bytes.len());
    assert!(
        entry["mtime_ms"].as_u64().unwrap() > 0,
        "mtime must be real; got {entry}"
    );
    assert_eq!(entry["sha256"], expected_sha);

    let read = http_get(&capsule.url, "/resources/files/report.md");
    assert_eq!(read.status, 200);
    assert_eq!(read.body, bytes.to_vec());
    assert_eq!(
        read.header("etag").unwrap(),
        format!("\"sha256:{expected_sha}\"")
    );
    assert_eq!(
        read.header("content-type").unwrap(),
        "application/octet-stream"
    );
    assert!(read.header("x-murmur-generation").is_some());
    assert_eq!(read.header("x-murmur-export-root").unwrap(), "out/");
    assert_eq!(
        read.header("x-murmur-containment").unwrap(),
        achieved_class_here().as_str(),
        "the plane must report the class the session actually achieved"
    );
    assert_eq!(
        listed["containment_achieved"],
        achieved_class_here().as_str()
    );

    // Ten more reads buy no inference turn: the count is the same before and after.
    let inferences_before = capsule.trace_events("inference").len();
    for _ in 0..10 {
        assert_eq!(
            http_get(&capsule.url, "/resources/files/report.md").status,
            200
        );
    }
    assert_eq!(
        capsule.trace_events("inference").len(),
        inferences_before,
        "a read must not cost an inference turn"
    );
    assert_eq!(capsule.trace_events("resource_read").len(), 11);
}

// ── Scenario 2: absent = deny ─────────────────────────────────────────────────

#[test]
fn an_undeclared_export_denies_both_verbs_and_records_the_denial() {
    if common::skip_without_host_support(
        "an_undeclared_export_denies_both_verbs_and_records_the_denial",
    ) {
        return;
    }
    let capsule = launch(None, 1);
    write_export_file(&capsule.project, "report.md", b"never served");

    for path in ["/resources/files", "/resources/files/report.md"] {
        let response = http_get(&capsule.url, path);
        assert_eq!(response.status, 404, "path {path}");
        assert_eq!(response.json()["error"], "no_resource_plane");
        assert!(
            !String::from_utf8_lossy(&response.body).contains("never served"),
            "no file bytes may appear in the refusal for {path}"
        );
    }

    let list = capsule.trace_events("resource_list");
    let read = capsule.trace_events("resource_read");
    assert_eq!(list.len(), 1, "trace: {}", capsule.trace());
    assert_eq!(read.len(), 1, "trace: {}", capsule.trace());
    assert_eq!(list[0]["outcome"], "no_resource_plane");
    assert_eq!(read[0]["outcome"], "no_resource_plane");
    assert_eq!(read[0]["sha256"], Value::Null);
    assert_eq!(read[0]["bytes"], Value::Null);
    assert!(read[0]["reason"].is_string());
}

// ── Scenario 3: traversal, including percent-encoded ──────────────────────────

#[test]
fn every_traversal_shape_is_refused_and_recorded() {
    if common::skip_without_host_support("every_traversal_shape_is_refused_and_recorded") {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    write_export_file(&capsule.project, "report.md", b"public");
    fs::write(capsule.project.join("secret.txt"), b"SECRET-MARKER-42").unwrap();

    let attempts = [
        "/resources/files/../secret.txt",
        "/resources/files/%2e%2e%2fsecret.txt",
        "/resources/files/out/../../secret.txt",
        "/resources/files//etc/passwd",
    ];
    for path in attempts {
        let response = http_get(&capsule.url, path);
        assert_eq!(response.status, 403, "path {path}: {:?}", response.json());
        assert_eq!(response.json()["error"], "outside_root", "path {path}");
        assert!(
            !String::from_utf8_lossy(&response.body).contains("SECRET-MARKER-42"),
            "the secret must not appear in the response for {path}"
        );
    }

    let refusals = capsule.trace_events("resource_read");
    assert_eq!(refusals.len(), attempts.len(), "trace: {}", capsule.trace());
    for event in &refusals {
        assert_eq!(event["outcome"], "outside_root");
        assert_eq!(event["sha256"], Value::Null);
    }
    // The refusal is recorded, not repaired: nothing normalised the request into a servable path.
    assert!(!capsule.trace().contains("SECRET-MARKER-42"));
}

// ── Scenario 4: escaping symlink ──────────────────────────────────────────────

#[test]
fn an_escaping_symlink_is_refused_at_whatever_class_this_host_achieves() {
    if common::skip_without_host_support(
        "an_escaping_symlink_is_refused_at_whatever_class_this_host_achieves",
    ) {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    write_export_file(&capsule.project, "report.md", b"public");
    fs::write(capsule.project.join("secret.txt"), b"SECRET-MARKER-42").unwrap();
    let secret_sha = murmur_artifact::sha256_hex(b"SECRET-MARKER-42");
    std::os::unix::fs::symlink(
        capsule.project.join("secret.txt"),
        capsule.project.join("out").join("escape.txt"),
    )
    .unwrap();

    let response = http_get(&capsule.url, "/resources/files/escape.txt");
    assert_eq!(response.status, 403, "body: {:?}", response.json());
    let expected = match achieved_class_here() {
        ContainmentClass::Scoped => "symlink_refused",
        _ => "outside_root",
    };
    assert_eq!(response.json()["error"], expected);
    assert!(!String::from_utf8_lossy(&response.body).contains("SECRET-MARKER-42"));

    let listed = http_get(&capsule.url, "/resources/files").json();
    let body = listed.to_string();
    assert!(
        !body.contains(&secret_sha),
        "the listing must never report the secret file's hash; got {body}"
    );
    assert!(!body.contains("escape.txt"), "got {body}");
}

// ── Scenario 5's unit-test substitute is in capsule-runtime; see the build summary ──

// ── Scenario 6: max_bytes ─────────────────────────────────────────────────────

#[test]
fn max_bytes_refuses_the_read_but_not_the_listing() {
    if common::skip_without_host_support("max_bytes_refuses_the_read_but_not_the_listing") {
        return;
    }
    let capsule = launch(
        Some("exports:\n  files:\n    root: out/\n    mode: read-only\n    max_bytes: 1Ki\n"),
        1,
    );
    write_export_file(&capsule.project, "small.txt", &[b'a'; 100]);
    write_export_file(&capsule.project, "big.bin", &[b'b'; 4096]);

    let listed = http_get(&capsule.url, "/resources/files").json();
    assert_eq!(listed["max_bytes"], 1024);
    let big = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "big.bin")
        .expect("an oversized file is still listed");
    assert_eq!(big["size_bytes"], 4096);

    let refused = http_get(&capsule.url, "/resources/files/big.bin");
    assert_eq!(refused.status, 413);
    assert_eq!(refused.json()["error"], "too_large");

    let served = http_get(&capsule.url, "/resources/files/small.txt");
    assert_eq!(served.status, 200);
    assert_eq!(served.body.len(), 100);

    let reads = capsule.trace_events("resource_read");
    let too_large = reads
        .iter()
        .find(|event| event["path"] == "big.bin")
        .expect("the refusal is recorded");
    assert_eq!(too_large["outcome"], "too_large");
    assert_eq!(too_large["bytes"], Value::Null);
    assert_eq!(too_large["sha256"], Value::Null);
}

// ── Scenario 7: no write path ─────────────────────────────────────────────────

#[test]
fn there_is_no_write_path() {
    if common::skip_without_host_support("there_is_no_write_path") {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    let bytes = b"original bytes";
    write_export_file(&capsule.project, "report.md", bytes);
    let original_sha = murmur_artifact::sha256_hex(bytes);

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = http_request(&capsule.url, method, "/resources/files/report.md");
        assert_eq!(response.status, 405, "{method}");
        assert_eq!(response.json()["error"], "method_not_allowed", "{method}");
        assert_eq!(response.header("allow"), Some("GET"), "{method}");
    }
    let response = http_request(&capsule.url, "PUT", "/resources/files/new.txt");
    assert_eq!(response.status, 405);
    assert_eq!(response.header("allow"), Some("GET"));

    assert_eq!(
        murmur_artifact::sha256_hex(&fs::read(capsule.project.join("out/report.md")).unwrap()),
        original_sha
    );
    assert!(!capsule.project.join("out/new.txt").exists());
}

// ── Scenario 8: generation ────────────────────────────────────────────────────

#[test]
fn the_generation_moves_with_turns_and_the_etag_moves_with_content() {
    if common::skip_without_host_support(
        "the_generation_moves_with_turns_and_the_etag_moves_with_content",
    ) {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 2);
    write_export_file(&capsule.project, "report.md", b"stable bytes");

    let before = http_get(&capsule.url, "/resources/files/report.md");
    assert_eq!(before.status, 200);
    assert_eq!(before.header("x-murmur-generation"), Some("0"));

    capsule.submit_and_await("gen-1", "do the thing");

    let after = http_get(&capsule.url, "/resources/files/report.md");
    assert_eq!(after.status, 200);
    assert_eq!(after.header("x-murmur-generation"), Some("1"));

    // The generation moved with the turn; the validator did not, because the bytes did not.
    assert_eq!(before.body, after.body);
    assert_eq!(before.header("etag"), after.header("etag"));

    assert_eq!(
        http_get(&capsule.url, "/resources/files").json()["generation"],
        1
    );
}

// ── Scenario 9: concurrent reads leave a parseable trace ──────────────────────

#[test]
fn concurrent_reads_leave_every_trace_line_parseable() {
    if common::skip_without_host_support("concurrent_reads_leave_every_trace_line_parseable") {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 2);
    let bytes = b"concurrent bytes";
    write_export_file(&capsule.project, "report.md", bytes);

    let response = http_post_json(&capsule.url, "/", &message_send_body("conc-1", "work"));
    let task_id = response["result"]["id"].as_str().unwrap().to_string();

    let readers: Vec<_> = (0..20)
        .map(|_| {
            let url = capsule.url.clone();
            thread::spawn(move || {
                let response = http_get(&url, "/resources/files/report.md");
                (response.status, response.body)
            })
        })
        .collect();
    for reader in readers {
        let (status, body) = reader.join().expect("reader thread should not panic");
        assert_eq!(status, 200);
        assert_eq!(body, bytes.to_vec());
    }

    capsule.await_task(&task_id);
    // Wait for the agent loop's own task_end to land before reading the file.
    let deadline = Instant::now() + Duration::from_secs(30);
    while capsule.trace_events("task_end").is_empty() {
        assert!(Instant::now() < deadline, "timed out waiting for task_end");
        thread::sleep(Duration::from_millis(50));
    }

    // Every line parses — this is the invariant the per-line flush and the O_APPEND handle exist
    // for. `trace_events` panics on a line that does not.
    let reads = capsule.trace_events("resource_read");
    assert_eq!(reads.len(), 20, "trace:\n{}", capsule.trace());
    assert!(reads.iter().all(|event| event["outcome"] == "ok"));
    assert!(!capsule.trace_events("task_start").is_empty());
    assert!(!capsule.trace_events("inference").is_empty());
    assert!(!capsule.trace_events("task_end").is_empty());
}

// ── Scenario 13: a read is never torn, and the etag always describes the body ──

/// Alternates two fixed 1 MiB payloads under `out/churn.bin` while the test reads it, and returns
/// the payload bytes. `rename_into_place` selects the authoring convention under test.
fn churn_writer(
    project: &Path,
    rename_into_place: bool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> thread::JoinHandle<()> {
    let out = project.join("out");
    fs::create_dir_all(&out).unwrap();
    let target = out.join("churn.bin");
    let temp = out.join(".churn.tmp");
    let payloads = [vec![0xAAu8; 1024 * 1024], vec![0xBBu8; 1024 * 1024]];
    fs::write(&target, &payloads[0]).unwrap();
    thread::spawn(move || {
        let mut index = 0usize;
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let payload = &payloads[index % 2];
            if rename_into_place {
                fs::write(&temp, payload).unwrap();
                fs::rename(&temp, &target).unwrap();
            } else {
                let mut file = fs::File::create(&target).unwrap();
                // Two writes with a yield between them, so an in-place rewrite has a real window
                // in which half of each payload is on disk.
                file.write_all(&payload[..payload.len() / 2]).unwrap();
                thread::yield_now();
                file.write_all(&payload[payload.len() / 2..]).unwrap();
            }
            index += 1;
        }
    })
}

#[test]
fn a_rename_into_place_rewrite_is_never_read_torn() {
    if common::skip_without_host_support("a_rename_into_place_rewrite_is_never_read_torn") {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = churn_writer(&capsule.project, true, std::sync::Arc::clone(&stop));

    for attempt in 0..200 {
        let response = http_get(&capsule.url, "/resources/files/churn.bin");
        assert_eq!(response.status, 200, "attempt {attempt}");
        assert_eq!(
            response.body.len(),
            1024 * 1024,
            "attempt {attempt}: a rename-into-place rewrite must never produce a short body"
        );
        let first = response.body[0];
        assert!(
            first == 0xAA || first == 0xBB,
            "attempt {attempt}: unexpected payload byte {first:#x}"
        );
        assert!(
            response.body.iter().all(|byte| *byte == first),
            "attempt {attempt}: a rename-into-place rewrite must never produce a mixed body"
        );
        assert_eq!(
            response.header("etag").unwrap(),
            format!("\"sha256:{}\"", murmur_artifact::sha256_hex(&response.body)),
            "attempt {attempt}: the etag must describe the body it was sent with"
        );
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}

/// The other half of the same experiment: with the writer truncating and rewriting in place, the
/// etag must still describe the body on every response — rules 1 and 2 hold regardless of how the
/// agent writes. Whether a body comes back torn is the documented consequence of ignoring the
/// rename-into-place convention, so it is observed rather than asserted either way.
#[test]
fn an_in_place_rewrite_still_yields_an_etag_that_describes_its_own_body() {
    if common::skip_without_host_support(
        "an_in_place_rewrite_still_yields_an_etag_that_describes_its_own_body",
    ) {
        return;
    }
    let capsule = launch(Some(EXPORTS_OUT), 1);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = churn_writer(&capsule.project, false, std::sync::Arc::clone(&stop));

    let mut torn = 0usize;
    for attempt in 0..200 {
        let response = http_get(&capsule.url, "/resources/files/churn.bin");
        assert_eq!(response.status, 200, "attempt {attempt}");
        assert_eq!(
            response.header("etag").unwrap(),
            format!("\"sha256:{}\"", murmur_artifact::sha256_hex(&response.body)),
            "attempt {attempt}: the etag must describe the body it was sent with"
        );
        let uniform =
            !response.body.is_empty() && response.body.iter().all(|byte| *byte == response.body[0]);
        if response.body.len() != 1024 * 1024 || !uniform {
            torn += 1;
        }
    }
    eprintln!(
        "in-place rewrite: {torn}/200 responses were short or mixed (documented consequence, not a defect)"
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}
