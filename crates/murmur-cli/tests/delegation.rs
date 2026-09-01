//! An agent handing one task to one sub-capsule, end to end.
//!
//! Every case runs the real thing: `mur-roost` on a loopback port over a real local artifact
//! store, a real parent capsule with a real listener, and a child launched by the parent's own
//! runtime as its own `mur run` process. Nothing about a delegation is stubbed — what a case
//! observes is what an operator observes, in the parent's `trace.jsonl`, in the child's directory
//! and in the text the model was handed back.
//!
//! One daemon, one registry and one `HOME` are shared by the whole file, because `HOME` and
//! `MURMUR_ROOST_URL` are process-wide and a child resolves both through the environment its
//! parent's launcher composes. Each case gets its own parent session and its own project
//! directory, so nothing a case creates is visible to another.
//!
//! The daemon is reached through a recording proxy, so a case can assert about the *actual*
//! credential the parent presented and the *actual* approval the daemon issued, rather than about
//! two tokens of the same shape minted for the harness.

#[path = "common/mod.rs"]
mod common;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use mur_roost::{authority::SpawnAuthority, State};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The capsule every delegating case in this file runs. One name, one published manifest: the
/// daemon derives a registrant's envelope from the registry, and the envelope is the same for
/// every case even though each case scripts its own model.
const PARENT: &str = "delegator";
/// The capsule of the one case that declares no `capabilities.spawn.allow`.
const UNGRANTED_PARENT: &str = "solo";
const VERSION: &str = "0.1.0";

/// The sub-capsule that answers. Its model always replies with [`WORKER_ANSWER`], so text that
/// reaches the parent's tool result came from the child and from nowhere else.
const WORKER: &str = "worker";
/// The sub-capsule that declares a grant its parent does not hold, so the referee refuses it.
const GREEDY_WORKER: &str = "greedy-worker";
/// The sub-capsule whose inference endpoint never answers, so its task never leaves `working`.
const MUTE_WORKER: &str = "mute-worker";

const WORKER_ANSWER: &str = "WORKER-ANSWER-4K2P-DELEGATED";

/// The bound every delegation in this file runs under, short enough that the silent-child case
/// finishes in a test and long enough that a launched child gets its turn.
const TIMEOUT_SECS: u64 = 20;

/// The two wire prefixes no token may ever be found behind.
const CREDENTIAL_PREFIX: &str = "msc1.";
const APPROVAL_PREFIX: &str = "msa1.";

// ── The daemon, behind a recording proxy ──────────────────────────────────────

/// `mur-roost` on a loopback port, with every byte in each direction passing through a proxy that
/// keeps the tokens it sees.
///
/// The proxy is what makes "no token reaches a workdir, a trace or the model" assertable about the
/// real tokens: a credential only ever appears in a request header and an approval only ever in a
/// response body, and both cross this socket.
struct RecordingRoost {
    /// The address the runtime is pointed at — the proxy's, not the daemon's.
    url: String,
    state: Arc<State>,
    credentials: Arc<Mutex<HashSet<String>>>,
    approvals: Arc<Mutex<HashSet<String>>>,
    spawn_requests: Arc<AtomicUsize>,
}

impl RecordingRoost {
    fn start(registry_path: &Path, spawn_allow: &[&str]) -> Self {
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path: registry_path.to_path_buf(),
            spawn_allow: spawn_allow.iter().map(|name| name.to_string()).collect(),
            max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
            max_concurrent: mur_roost::bounds::DEFAULT_MAX_CONCURRENT,
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });

        let daemon = TcpListener::bind("127.0.0.1:0").unwrap();
        let daemon_addr = daemon.local_addr().unwrap().to_string();
        let daemon_state = Arc::clone(&state);
        thread::spawn(move || {
            for stream in daemon.incoming().flatten() {
                let state = Arc::clone(&daemon_state);
                thread::spawn(move || mur_roost::handle_connection(stream, state));
            }
        });

        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        let credentials = Arc::new(Mutex::new(HashSet::new()));
        let approvals = Arc::new(Mutex::new(HashSet::new()));
        let spawn_requests = Arc::new(AtomicUsize::new(0));

        let (seen_credentials, seen_approvals, counted) = (
            Arc::clone(&credentials),
            Arc::clone(&approvals),
            Arc::clone(&spawn_requests),
        );
        thread::spawn(move || {
            for stream in proxy.incoming().flatten() {
                let upstream = daemon_addr.clone();
                let (seen_credentials, seen_approvals, counted) = (
                    Arc::clone(&seen_credentials),
                    Arc::clone(&seen_approvals),
                    Arc::clone(&counted),
                );
                thread::spawn(move || {
                    relay(
                        stream,
                        &upstream,
                        &seen_credentials,
                        &seen_approvals,
                        &counted,
                    );
                });
            }
        });

        Self {
            url,
            state,
            credentials,
            approvals,
            spawn_requests,
        }
    }

    fn tokens(&self) -> Vec<String> {
        let mut tokens: Vec<String> = self.credentials.lock().unwrap().iter().cloned().collect();
        tokens.extend(self.approvals.lock().unwrap().iter().cloned());
        tokens
    }

    fn spawn_requests(&self) -> usize {
        self.spawn_requests.load(Ordering::SeqCst)
    }

    fn publish(&self, name: &str, version: &str, body: &str, component: Option<&Path>) {
        publish_capsule(&self.state.registry_path, name, version, body, component);
    }
}

/// Forward one request and its response, keeping whatever token each carried.
fn relay(
    mut client: TcpStream,
    upstream_addr: &str,
    credentials: &Mutex<HashSet<String>>,
    approvals: &Mutex<HashSet<String>>,
    spawn_requests: &AtomicUsize,
) {
    let Some(request) = read_framed_request(&mut client) else {
        return;
    };
    let head = String::from_utf8_lossy(&request).to_string();
    if head.starts_with("POST /spawn ") {
        spawn_requests.fetch_add(1, Ordering::SeqCst);
    }
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name
                .trim()
                .eq_ignore_ascii_case(capsule_runtime::SPAWN_CREDENTIAL_HEADER)
            {
                credentials.lock().unwrap().insert(value.trim().to_string());
            }
        }
    }

    let Ok(mut upstream) = TcpStream::connect(upstream_addr) else {
        return;
    };
    if upstream.write_all(&request).is_err() {
        return;
    }
    let _ = upstream.flush();

    let mut response = Vec::new();
    let _ = upstream.read_to_end(&mut response);
    let body = String::from_utf8_lossy(&response).to_string();
    if let Some(approval) = extract_json_string(&body, "approval") {
        approvals.lock().unwrap().insert(approval);
    }
    let _ = client.write_all(&response);
    let _ = client.flush();
}

/// One HTTP request read to its declared length, so the proxy never waits on a client that is
/// waiting on it.
fn read_framed_request(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < header_end + 4 + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(buffer)
}

/// The value of `"<key>":"..."` in `text`, for the one field the daemon answers a token in.
fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = text.find(&needle)? + needle.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

// ── Artifacts ─────────────────────────────────────────────────────────────────

fn publish_capsule(
    registry_root: &Path,
    name: &str,
    version: &str,
    manifest_body: &str,
    component_path: Option<&Path>,
) {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(format!("name: {name}\nversion: {version}\n{manifest_body}").as_bytes())
            .unwrap();
        if let Some(component_path) = component_path {
            zip.start_file("capsule.wasm", options).unwrap();
            zip.write_all(&std::fs::read(component_path).unwrap())
                .unwrap();
        }
        zip.finish().unwrap();
    }
    murmur_artifact::Registry::publish(
        &LocalRegistry::new(registry_root),
        murmur_artifact::ArtifactMeta {
            name: name.to_string(),
            version: version.to_string(),
            runtime: murmur_artifact::RuntimeType::Wasm,
            artifact_runtime: "capsule".to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
        },
        &cursor.into_inner(),
    )
    .unwrap();
}

fn publish_driver(registry_root: &Path) {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(
            format!("name: {DRIVER}\nversion: {DRIVER_VERSION}\nruntime: driver\n").as_bytes(),
        )
        .unwrap();
        zip.start_file("tool.wasm", options).unwrap();
        zip.write_all(
            &std::fs::read(common::fixture_path(
                "drivers/anthropic/driver/murmur-driver-anthropic.wasm",
            ))
            .unwrap(),
        )
        .unwrap();
        zip.finish().unwrap();
    }
    murmur_artifact::Registry::publish(
        &LocalRegistry::new(registry_root),
        murmur_artifact::ArtifactMeta {
            name: DRIVER.to_string(),
            version: DRIVER_VERSION.to_string(),
            runtime: murmur_artifact::RuntimeType::Wasm,
            artifact_runtime: "driver".to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
        },
        &cursor.into_inner(),
    )
    .unwrap();
}

// ── Endpoints ─────────────────────────────────────────────────────────────────

/// An inference endpoint that answers every request with the same text, for a sub-capsule whose
/// only job is to have an answer.
fn always_replying(text: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let body = end_turn_response(text);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = read_framed_request(&mut stream);
            let raw = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(raw.as_bytes());
            let _ = stream.flush();
        }
    });
    endpoint
}

/// An inference endpoint that accepts a connection and never answers on it, for a sub-capsule that
/// goes silent mid-task.
fn never_replying() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            // Held rather than dropped: a closed connection would fail the child's turn, and the
            // case is about a child that never answers, not one that errors.
            held.push(stream);
        }
    });
    endpoint
}

/// A stand-in inference endpoint whose responses are a queue the test pushes into while the
/// capsule runs, so a turn can be scripted after the capsule is already up.
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
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let raw = read_framed_request(&mut stream).unwrap_or_default();
                let text = String::from_utf8_lossy(&raw).to_string();
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                seen.lock().unwrap().push(
                    serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({"_raw": body})),
                );

                // Wait for the test to say what this turn answers. A turn the test has not
                // scripted is a turn it did not expect, and blocking here surfaces that as the
                // test's own timeout rather than as a confusing driver error.
                let deadline = Instant::now() + Duration::from_secs(120);
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

// ── The suite ─────────────────────────────────────────────────────────────────

struct Suite {
    roost: RecordingRoost,
    registry: PathBuf,
    /// Kept for the life of the process: `HOME` points inside it.
    _home: TempDir,
}

/// The one daemon, registry, `HOME` and delegation bound every case shares.
fn suite() -> &'static Suite {
    static SUITE: OnceLock<Suite> = OnceLock::new();
    SUITE.get_or_init(|| {
        let home = TempDir::new().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var(capsule_runtime::MUR_BINARY_ENV, mur_binary());
        std::env::set_var(
            capsule_runtime::DELEGATION_TIMEOUT_ENV,
            TIMEOUT_SECS.to_string(),
        );

        let registry = home.path().join(".murmur").join("artifacts");
        std::fs::create_dir_all(&registry).unwrap();
        publish_driver(&registry);

        // The daemon's own list gates a top-level registrant, which every parent here is.
        let roost = RecordingRoost::start(&registry, &[PARENT, UNGRANTED_PARENT]);
        std::env::set_var("MURMUR_ROOST_URL", &roost.url);

        // The parent's envelope, and the only thing about the parent the daemon reads. Loopback
        // with no port covers every port on it, so one published manifest covers a case whose
        // model endpoint is bound after this ran. The declaration carries no shell grant, which
        // is what `greedy-worker` exceeds.
        roost.publish(
            PARENT,
            VERSION,
            &format!(
                "artifacts: []\ncapabilities:\n  \
                 network:\n    allow: [127.0.0.1]\n  \
                 spawn:\n    allow: [{WORKER}, {GREEDY_WORKER}, {MUTE_WORKER}]\n"
            ),
            Some(&common::fixture_path(
                "run/components/capsule-env-echo.wasm",
            )),
        );

        // The sub-capsule that answers. It stays up between tasks so it is still reachable when
        // its parent reads the answer, which is what an A2A `tasks/get` needs.
        roost.publish(
            WORKER,
            VERSION,
            &agent_capsule_manifest(&always_replying(WORKER_ANSWER)),
            None,
        );
        // The sub-capsule the referee refuses: one grant beyond its parent's envelope. It never
        // launches, so its component only has to resolve.
        roost.publish(
            GREEDY_WORKER,
            VERSION,
            "artifacts: []\ncapabilities:\n  shell:\n    allow: [bash]\n",
            Some(&common::fixture_path(
                "run/components/capsule-env-echo.wasm",
            )),
        );
        // The sub-capsule that goes quiet: it binds, accepts the task, and its model never
        // answers, so the task never leaves `working`.
        roost.publish(
            MUTE_WORKER,
            VERSION,
            &agent_capsule_manifest(&never_replying()),
            None,
        );

        Suite {
            roost,
            registry,
            _home: home,
        }
    })
}

/// The `mur` binary a parent launches its children with, built up to date.
fn mur_binary() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| assert_cmd::cargo::cargo_bin("mur"))
        .clone()
}

/// A sub-capsule that serves A2A and answers from `endpoint`.
///
/// `after_task: sleep` is not decoration: a delegation reads its answer with an A2A `tasks/get`,
/// so the capsule that produced it has to still be listening when the read happens.
fn agent_capsule_manifest(endpoint: &str) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
         capabilities:\n  network:\n    allow: [{authority}]\n\
         lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n\
         inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
         api_key: test-key\n  driver:\n    artifact: {DRIVER}\n",
        authority = endpoint.trim_start_matches("http://"),
    )
}

// ── The parent under test ─────────────────────────────────────────────────────

struct Parent {
    project: PathBuf,
    session_dir: PathBuf,
    url: String,
    server: QueuedServer,
    handle: Option<thread::JoinHandle<capsule_runtime::LaunchResult>>,
}

impl Parent {
    /// Launch one parent capsule in this process, with its own scripted model.
    fn launch(name: &str, spawn_yaml: &str) -> Self {
        let suite = suite();
        let server = QueuedServer::start();
        let project = TempDir::new().unwrap().keep();

        let manifest_body = format!(
            "name: {name}\nversion: {VERSION}\n\
             artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             capabilities:\n  network:\n    allow: [127.0.0.1]\n{spawn_yaml}\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n\
             inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
             api_key: test-key\n  driver:\n    artifact: {DRIVER}\n",
            endpoint = server.endpoint,
        );
        let manifest_path = project.join("murmur.yaml");
        std::fs::write(&manifest_path, manifest_body).unwrap();
        let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();

        let staged = stage_session(
            Arc::new(LocalRegistry::new(&suite.registry)),
            stage_request(&project, &runtime_manifest),
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
            .recv_timeout(Duration::from_secs(120))
            .expect("timed out waiting for the capsule URL");

        Self {
            project,
            session_dir,
            url,
            server,
            handle: Some(handle),
        }
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.session_dir.join("trace.jsonl")).unwrap_or_default()
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

    /// The directories the parent composed for its children, if any.
    fn child_dirs(&self) -> Vec<PathBuf> {
        let children = self.project.join(".murmur").join("children");
        let Ok(entries) = std::fs::read_dir(children) else {
            return Vec::new();
        };
        entries.flatten().map(|entry| entry.path()).collect()
    }

    fn submit(&self, message_id: &str, text: &str) -> String {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {"message": {"messageId": message_id, "role": "user",
                                   "parts": [{"text": text}]}}
        })
        .to_string();
        let response = post_json(&self.url, &body);
        response["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a task id; got: {response}"))
            .to_string()
    }

    fn await_task(&self, task_id: &str, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let state = self.task_state(task_id);
            if state == "completed" || state == "failed" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for task {task_id}; last state: {state}"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn task_state(&self, task_id: &str) -> String {
        let body = json!({
            "jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}
        })
        .to_string();
        post_json(&self.url, &body)["result"]["status"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// One delegation turn: the model calls `delegate-task`, then ends its turn. Returns the tool
    /// result text the runtime fed back, with the untrusted-content fence stripped.
    fn delegate(&self, tool_id: &str, capsule: &str, version: &str, task: &str) -> String {
        self.server.push(tool_use_response(
            tool_id,
            "delegate-task",
            json!({"capsule": capsule, "version": version, "task": task}),
        ));
        self.server.push(end_turn_response("delegated"));
        let task_id = self.submit(&format!("msg-{tool_id}"), "delegate it");
        self.await_task(&task_id, Duration::from_secs(300));

        tool_result_text(&self.server.requests(), tool_id).unwrap_or_else(|| {
            panic!(
                "no tool_result for {tool_id}; requests: {:?}",
                self.server.requests()
            )
        })
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        // A `queue` + `sleep` capsule waits indefinitely for the next task, so the launch thread
        // is abandoned rather than joined — the same shape `peer_handoff.rs` uses.
        drop(self.handle.take());
    }
}

fn stage_request(
    project: &Path,
    runtime_manifest: &murmur_artifact::RuntimeManifest,
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
        internal_port: None,
        declared_containment_floor: ContainmentClass::Advisory,
        exports: runtime_manifest.exports.clone(),
        spawn_grant: None,
    }
}

// ── Raw HTTP ──────────────────────────────────────────────────────────────────

fn post_json(url: &str, body: &str) -> Value {
    let authority = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(authority).expect("should connect to the capsule");
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|_| json!({"_raw": body}))
}

/// `GET /.well-known/agent-card.json`, returning the status line's code.
fn agent_card_status(url: &str) -> u16 {
    let authority = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(authority).expect("should connect to the capsule");
    let request = format!(
        "GET /.well-known/agent-card.json HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    raw.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line: {raw}"))
}

/// The text of the tool result the runtime fed back for `tool_id`, with the fence stripped.
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

/// Return what the fence wrapped, for a result that carries one. A dispatch that never reached
/// the tool — a referee's refusal — comes back as the runtime's own text and carries none.
fn unfence(text: &str) -> String {
    let Some((open, rest)) = text.split_once('\n') else {
        return text.to_string();
    };
    if !open.starts_with("<untrusted-content source=tool:") || !open.ends_with('>') {
        return text.to_string();
    }
    rest.strip_suffix("\n</untrusted-content>")
        .unwrap_or_else(|| panic!("a fenced tool result must end at the closing marker: {text}"))
        .to_string()
}

/// Every file under `root`, following directories.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// The first file under `root` whose bytes contain `needle`.
fn find_in_files(root: &Path, needle: &str) -> Option<PathBuf> {
    files_under(root).into_iter().find(|path| {
        std::fs::read(path)
            .map(|bytes| {
                String::from_utf8_lossy(&bytes).contains(needle)
                    || bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
            })
            .unwrap_or(false)
    })
}

const SPAWN_YAML: &str = "  spawn:\n    allow: [worker, greedy-worker, mute-worker]\n";

// ── Cases ─────────────────────────────────────────────────────────────────────

/// The happy path, and the leak sweep over what it left behind.
///
/// The agent names a capsule, a version and a task. It is handed back the sub-capsule's own answer
/// — text no runtime composed — and neither workdir, neither trace nor the model's own context
/// holds a token of either kind.
#[test]
fn a_task_crosses_to_a_sub_capsule_and_its_answer_comes_back() {
    if common::skip_without_host_support(
        "a_task_crosses_to_a_sub_capsule_and_its_answer_comes_back",
    ) {
        return;
    }
    let suite = suite();
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    // The tool the grant put in the workdir, and the enum that is the whole of what stops the
    // model naming a capsule the operator never granted.
    let manifest = std::fs::read_to_string(
        parent
            .session_dir
            .join("tools")
            .join("delegate-task")
            .join("murmur.yaml"),
    )
    .expect("a granted capsule is written a delegate-task manifest");
    let declared: serde_yaml::Value = serde_yaml::from_str(&manifest).unwrap();
    let schema: Value = serde_json::from_str(declared["input_schema"].as_str().unwrap()).unwrap();
    assert_eq!(
        schema["required"],
        json!(["capsule", "version", "task"]),
        "{schema}"
    );
    assert_eq!(
        schema["properties"]["capsule"]["enum"],
        json!([WORKER, GREEDY_WORKER, MUTE_WORKER]),
        "the enum is this capsule's own spawn.allow"
    );

    let text = parent.delegate("toolu_happy", WORKER, VERSION, "summarise the report");
    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));

    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["capsule"], WORKER, "{result}");
    assert_eq!(result["version"], VERSION, "{result}");
    assert_eq!(
        result["output"].as_str().unwrap_or_default().trim(),
        WORKER_ANSWER,
        "the answer is the child's own text"
    );
    let delegation_id = result["delegation_id"].as_str().unwrap_or_default();
    assert!(delegation_id.starts_with("dlg_"), "{result}");
    assert!(
        result["session_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("ses_"),
        "{result}"
    );

    // A directory of the child's own, beneath the parent's accessible workdir, with its own trace.
    let child_dirs = parent.child_dirs();
    assert_eq!(child_dirs.len(), 1, "{child_dirs:?}");
    let child_dir = &child_dirs[0];
    assert!(
        child_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!("{WORKER}-"))),
        "{child_dir:?}"
    );
    assert!(
        find_in_files(child_dir, "\"event_type\":\"session_start\"").is_some(),
        "the child kept a trace of its own under {}",
        child_dir.display()
    );

    // One `delegation` event, naming the same delegation the model was told about.
    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["outcome"], "completed", "{event}");
    assert_eq!(event["delegation_id"], delegation_id, "{event}");
    assert_eq!(event["capsule"], WORKER, "{event}");
    assert_eq!(event["version"], VERSION, "{event}");
    assert_eq!(event["child_session_id"], result["session_id"], "{event}");
    assert_eq!(event["reason"], Value::Null, "{event}");

    // The sweep. Both workdirs, both traces, and every request the parent's model ever saw.
    let tokens = suite.roost.tokens();
    assert!(
        tokens
            .iter()
            .any(|token| token.starts_with(CREDENTIAL_PREFIX)),
        "the harness must have seen a real credential to assert about: {tokens:?}"
    );
    assert!(
        tokens
            .iter()
            .any(|token| token.starts_with(APPROVAL_PREFIX)),
        "the harness must have seen a real approval to assert about: {tokens:?}"
    );

    let model_context = serde_json::to_string(&parent.server.requests()).unwrap();
    let mut needles: Vec<String> = vec![CREDENTIAL_PREFIX.to_string(), APPROVAL_PREFIX.to_string()];
    needles.extend(tokens);
    for needle in needles {
        if let Some(path) = find_in_files(&parent.project, &needle) {
            panic!("'{needle}' reached {}", path.display());
        }
        if let Some(path) = find_in_files(&parent.session_dir, &needle) {
            panic!("'{needle}' reached {}", path.display());
        }
        assert!(
            !model_context.contains(&needle),
            "'{needle}' reached the model's context"
        );
        assert!(
            !text.contains(&needle),
            "'{needle}' reached the tool result"
        );
    }
}

/// A capsule that declares no `capabilities.spawn.allow` is not offered a tool that fails — it is
/// offered no tool at all.
#[test]
fn a_capsule_without_the_grant_is_never_offered_the_tool() {
    if common::skip_without_host_support("a_capsule_without_the_grant_is_never_offered_the_tool") {
        return;
    }
    let parent = Parent::launch(UNGRANTED_PARENT, "");

    // Not in the workdir.
    assert!(
        !parent
            .session_dir
            .join("tools")
            .join("delegate-task")
            .exists(),
        "an ungranted capsule is written no delegate-task manifest"
    );

    // Nor in what the model was ever shown, across a real turn. Run first, because the trace's
    // session frame is written by the loop rather than by the bind the launch reports on.
    parent.server.push(end_turn_response("nothing to delegate"));
    let task_id = parent.submit("msg-ungranted", "do something local");
    parent.await_task(&task_id, Duration::from_secs(120));
    assert!(
        !parent.server.offered_tools().contains("delegate-task"),
        "offered: {:?}",
        parent.server.offered_tools()
    );

    // Nor in what the session declared it could do.
    let starts = parent.events("session_start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    let declared: Vec<&str> = starts[0]["tools_declared"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        !declared.contains(&"delegate-task"),
        "tools_declared: {declared:?}"
    );

    // And nothing about the run is a delegation refusal, because nothing offered the tool.
    assert!(parent.events("delegation").is_empty());
    assert!(parent.child_dirs().is_empty());
}

/// A referee's refusal reaches the model as the referee's own sentence: the manifest key and the
/// offending entry, with no HTTP transcript around it and no child anywhere.
#[test]
fn a_referee_refusal_names_the_axis_and_the_entry() {
    if common::skip_without_host_support("a_referee_refusal_names_the_axis_and_the_entry") {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);
    let before = suite().roost.spawn_requests();

    let text = parent.delegate("toolu_refused", GREEDY_WORKER, VERSION, "overreach");

    assert!(
        text.contains("capabilities.shell.allow"),
        "the refusal names the axis: {text}"
    );
    assert!(text.contains("bash"), "the refusal names the entry: {text}");
    assert!(
        !text.contains("HTTP") && !text.contains("Forbidden") && !text.contains("content-type"),
        "the refusal carries no HTTP transcript: {text}"
    );

    // The daemon was asked, and answered no: nothing was launched and nothing was composed. The
    // counter is the shared daemon's, so this is a floor rather than an equality — another case
    // running beside this one is asking it questions too.
    assert!(
        suite().roost.spawn_requests() > before,
        "the refusal came from the daemon, so the daemon was asked"
    );
    assert!(parent.child_dirs().is_empty(), "no child directory exists");

    // A refused tool call is a tool result: the parent's own run carried on.
    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "refused", "{}", events[0]);
    assert_eq!(events[0]["delegation_id"], Value::Null, "{}", events[0]);
    assert!(events[0]["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("capabilities.shell.allow"));
}

/// A child that never answers fails the call. It does not hang the parent, and it does not
/// survive the delegation that gave up on it.
#[test]
fn a_silent_child_fails_the_call_rather_than_hanging_the_parent() {
    if common::skip_without_host_support(
        "a_silent_child_fails_the_call_rather_than_hanging_the_parent",
    ) {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    let started = Instant::now();
    let text = parent.delegate("toolu_mute", MUTE_WORKER, VERSION, "never answer this");
    let elapsed = started.elapsed();

    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));
    assert_eq!(result["status"], "timed_out", "{result}");
    assert!(
        result["output"]
            .as_str()
            .unwrap_or_default()
            .contains(MUTE_WORKER),
        "the failure names the capsule: {result}"
    );
    assert!(
        elapsed < Duration::from_secs(TIMEOUT_SECS) + Duration::from_secs(180),
        "the call returned rather than hanging, in {elapsed:?}"
    );

    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "timed_out", "{}", events[0]);

    // The parent is still its own capsule afterwards: it answers the next turn.
    parent.server.push(end_turn_response("still here"));
    let task_id = parent.submit("msg-after-timeout", "are you there");
    parent.await_task(&task_id, Duration::from_secs(120));
    assert_eq!(parent.task_state(&task_id), "completed");
}

/// The parent stays reachable while a child runs: the wait is on a blocking thread, not on the
/// `LocalSet` its own listener runs on.
#[test]
fn the_parent_answers_its_card_while_a_delegation_is_in_flight() {
    if common::skip_without_host_support(
        "the_parent_answers_its_card_while_a_delegation_is_in_flight",
    ) {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    parent.server.push(tool_use_response(
        "toolu_inflight",
        "delegate-task",
        json!({"capsule": MUTE_WORKER, "version": VERSION, "task": "hold the line"}),
    ));
    parent.server.push(end_turn_response("delegated"));
    let task_id = parent.submit("msg-inflight", "delegate it");

    // Wait for the delegation to actually be under way — the child has to be launched before the
    // parent is holding a turn open on it.
    let deadline = Instant::now() + Duration::from_secs(240);
    while parent.child_dirs().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the child was never launched; task state: {}",
            parent.task_state(&task_id)
        );
        thread::sleep(Duration::from_millis(200));
    }

    // The listener answers, repeatedly, for as long as the delegation is in flight.
    for _ in 0..5 {
        assert_eq!(
            agent_card_status(&parent.url),
            200,
            "the parent's card must answer while its delegation runs"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(parent.task_state(&task_id), "working");

    parent.await_task(&task_id, Duration::from_secs(300));
}
