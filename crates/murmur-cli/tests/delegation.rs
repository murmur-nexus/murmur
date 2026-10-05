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

use common::{never_replying, tool_result_text, tool_use_response};

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
/// reaches the parent came from the child and from nowhere else.
const WORKER: &str = "worker";
/// Two more of the same, so a fan-out case can name three distinct capsules and tell the three
/// delegations apart by more than their ids.
const WORKER_TWO: &str = "worker-two";
const WORKER_THREE: &str = "worker-three";
/// The sub-capsule that declares a grant its parent does not hold, so the referee refuses it.
const GREEDY_WORKER: &str = "greedy-worker";
/// The sub-capsule whose inference endpoint never answers, so its task never leaves `working` and
/// its session never ends.
const MUTE_WORKER: &str = "mute-worker";
/// The sub-capsule the recording proxy refuses on the daemon's behalf, with the daemon's own
/// depth-bound sentence. Never launched, so nothing about it beyond the name matters.
const DEEP_WORKER: &str = "deep-worker";

/// The sub-capsule declaring a host variable its parent does not, so the referee refuses it on the
/// env axis.
const THIRSTY_WORKER: &str = "thirsty-worker";

/// The sub-capsule that answers as [`WORKER`] does from behind a door declaring
/// `network.authentication`, so its parent must present its operator token to drive it.
const AUTH_WORKER: &str = "auth-worker";

/// The host variable both the parent and [`WORKER`] declare, and the whole of what a delegated
/// child is handed beyond the names the runtime owns.
const PROVIDER_KEY_VAR: &str = "MURMUR_TEST_PROVIDER_KEY";
/// Its value: distinctive enough that a substring sweep over a whole workdir tree, and over every
/// request the parent's model saw, is a meaningful search.
const PROVIDER_KEY_VALUE: &str = "sk-test-PROVIDERKEY-8W3M-DELEGATED";
/// The variable [`THIRSTY_WORKER`] declares and no parent here does.
const UNGRANTED_VAR: &str = "MURMUR_TEST_UNGRANTED_KEY";

const WORKER_ANSWER: &str = "WORKER-ANSWER-4K2P-DELEGATED";

/// The bound every delegation in this file runs under, short enough that the wedged-child case
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
    /// Capsule name → the sentence a `POST /spawn` naming it is refused with, answered by the
    /// proxy instead of being relayed.
    ///
    /// Keyed by capsule name rather than armed as a one-shot because one daemon is shared by
    /// every case in this file and they run concurrently: a refusal armed for one case must not
    /// be able to land on another's spawn.
    refusals: Arc<Mutex<HashMap<String, String>>>,
}

impl RecordingRoost {
    fn start(registry_path: &Path, spawn_allow: &[&str]) -> Self {
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path: registry_path.to_path_buf(),
            spawn_allow: spawn_allow.iter().map(|name| name.to_string()).collect(),
            max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
            max_concurrent: mur_roost::bounds::DEFAULT_MAX_CONCURRENT,
            // One daemon serves every case in this suite, so its host census is the suite's total.
            max_live_capsules: u32::MAX,
            inherited: Default::default(),
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
        let refusals = Arc::new(Mutex::new(HashMap::new()));

        let (seen_credentials, seen_approvals, counted, canned) = (
            Arc::clone(&credentials),
            Arc::clone(&approvals),
            Arc::clone(&spawn_requests),
            Arc::clone(&refusals),
        );
        thread::spawn(move || {
            for stream in proxy.incoming().flatten() {
                let upstream = daemon_addr.clone();
                let (seen_credentials, seen_approvals, counted, canned) = (
                    Arc::clone(&seen_credentials),
                    Arc::clone(&seen_approvals),
                    Arc::clone(&counted),
                    Arc::clone(&canned),
                );
                thread::spawn(move || {
                    relay(
                        stream,
                        &upstream,
                        &seen_credentials,
                        &seen_approvals,
                        &counted,
                        &canned,
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
            refusals,
        }
    }

    /// Answer every `POST /spawn` naming `capsule` with `sentence`, in the shape the daemon
    /// refuses in, instead of relaying it upstream.
    fn refuse_spawns_of(&self, capsule: &str, sentence: &str) {
        self.refusals
            .lock()
            .unwrap()
            .insert(capsule.to_string(), sentence.to_string());
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
        common::publish_to_store(
            &self.state.registry_path,
            name,
            version,
            "capsule",
            body,
            component.map(|path| ("capsule.wasm", path)),
        );
    }
}

/// Forward one request and its response, keeping whatever token each carried.
fn relay(
    mut client: TcpStream,
    upstream_addr: &str,
    credentials: &Mutex<HashSet<String>>,
    approvals: &Mutex<HashSet<String>>,
    spawn_requests: &AtomicUsize,
    refusals: &Mutex<HashMap<String, String>>,
) {
    let Some(request) = read_framed_request(&mut client) else {
        return;
    };
    let head = String::from_utf8_lossy(&request).to_string();
    if head.starts_with("POST /spawn ") {
        spawn_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(name) = extract_json_string(&head, "name") {
            if let Some(sentence) = refusals.lock().unwrap().get(&name).cloned() {
                let body = json!({ "error": sentence }).to_string();
                let raw = format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = client.write_all(raw.as_bytes());
                let _ = client.flush();
                return;
            }
        }
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

fn publish_driver(registry_root: &Path) {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(
            format!(
                "name: {DRIVER}\nversion: {DRIVER_VERSION}\nruntime: driver\n\
                 upstream_auth:\n  header: x-api-key\n  value: \"{{key}}\"\n"
            )
            .as_bytes(),
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
            wit_contracts: None,
        },
        &cursor.into_inner(),
    )
    .unwrap();
}

// ── Endpoints ─────────────────────────────────────────────────────────────────

/// An inference endpoint that answers every request with the same text, for a sub-capsule whose
/// only job is to have an answer.
///
/// Every request is kept as the bytes it arrived as, so a case can assert what a child put on the
/// wire — a resolved key travels in a header, which a parsed body would not carry.
struct AlwaysReplying {
    endpoint: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl AlwaysReplying {
    /// The raw bytes of every request this endpoint has received.
    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

fn always_replying(text: &str) -> AlwaysReplying {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let body = end_turn_response(text);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            if let Some(raw) = read_framed_request(&mut stream) {
                seen.lock().unwrap().push(raw);
            }
            let raw = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(raw.as_bytes());
            let _ = stream.flush();
        }
    });
    AlwaysReplying { endpoint, requests }
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

    /// Drop every scripted response no turn took.
    fn discard_unused(&self) {
        self.responses.lock().unwrap().clear();
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
    /// The answering sub-capsule's own inference endpoint, held so a case can read what that
    /// child put on the wire.
    worker_inference: AlwaysReplying,
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
        // The provider key every capsule here references rather than bakes in. Set before
        // anything launches, because the daemon resolves a child manifest's `${VAR}` against this
        // same process environment when it referees a spawn.
        std::env::set_var(PROVIDER_KEY_VAR, PROVIDER_KEY_VALUE);

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
                 env:\n    allow: [{PROVIDER_KEY_VAR}]\n  \
                 spawn:\n    allow: [{WORKER}, {WORKER_TWO}, {WORKER_THREE}, {GREEDY_WORKER}, \
                 {MUTE_WORKER}, {DEEP_WORKER}, {THIRSTY_WORKER}, {AUTH_WORKER}]\n"
            ),
            Some(&common::fixture_path(
                "run/components/capsule-env-echo.wasm",
            )),
        );

        // The sub-capsules that answer. Each takes one task and exits, which is what makes its
        // outcome reach its parent: a delegated session reports its completion when the session
        // ends, so a capsule that sleeps between tasks never ends and never reports.
        let worker_inference = always_replying(WORKER_ANSWER);
        roost.publish(
            WORKER,
            VERSION,
            &agent_capsule_manifest(&worker_inference.endpoint),
            None,
        );
        // Two more of the same, so a fan-out case can tell the three delegations apart by more
        // than their ids.
        for name in [WORKER_TWO, WORKER_THREE] {
            roost.publish(
                name,
                VERSION,
                &agent_capsule_manifest(
                    &always_replying(&format!("{WORKER_ANSWER}-{name}")).endpoint,
                ),
                None,
            );
        }
        // The same answer from behind an authenticated door.
        roost.publish(
            AUTH_WORKER,
            VERSION,
            &format!(
                "{}network:\n  authentication:\n    scheme: bearer\n",
                agent_capsule_manifest(&always_replying(WORKER_ANSWER).endpoint)
            ),
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
        // answers, so the task never leaves `working` and the session never ends. Nothing but the
        // delegation deadline ever stops it.
        roost.publish(
            MUTE_WORKER,
            VERSION,
            &agent_capsule_manifest_with(
                &never_replying(),
                "task_acceptance: queue\n  after_task: sleep",
            ),
            None,
        );
        // The sub-capsule a bound refuses. Published so the parent's own manifest can name it;
        // the proxy answers its spawn before the daemon resolves anything.
        roost.publish(
            DEEP_WORKER,
            VERSION,
            "artifacts: []\n",
            Some(&common::fixture_path(
                "run/components/capsule-env-echo.wasm",
            )),
        );

        // The sub-capsule the referee refuses on the env axis: one host variable beyond its
        // parent's declaration. It never launches, so its component only has to resolve.
        roost.publish(
            THIRSTY_WORKER,
            VERSION,
            &format!("artifacts: []\ncapabilities:\n  env:\n    allow: [{UNGRANTED_VAR}]\n"),
            Some(&common::fixture_path(
                "run/components/capsule-env-echo.wasm",
            )),
        );

        Suite {
            roost,
            registry,
            worker_inference,
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

/// A sub-capsule that serves A2A, answers from `endpoint`, and then goes.
///
/// `after_task: exit` is not decoration: a delegated session posts its completion when the session
/// ends, so a sub-capsule that sleeps between tasks is one whose parent hears nothing until the
/// delegation deadline stops it.
fn agent_capsule_manifest(endpoint: &str) -> String {
    agent_capsule_manifest_with(endpoint, "task_acceptance: single\n  after_task: exit")
}

/// The same, under a stated `lifecycle:` body — for the sub-capsule that has to stay up rather
/// than exit.
fn agent_capsule_manifest_with(endpoint: &str, lifecycle: &str) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
         \x20   gateway:\n      endpoint: {endpoint}\n      \
         api_key: ${{{PROVIDER_KEY_VAR}}}\n\
         capabilities:\n  network:\n    allow: [{authority}]\n  \
         env:\n    allow: [{PROVIDER_KEY_VAR}]\n\
         lifecycle:\n  {lifecycle}\n\
         inference:\n  transport: http\n  model: test-model\n  \
         driver:\n    artifact: {DRIVER}\n",
        authority = endpoint.trim_start_matches("http://"),
    )
}

// ── The parent under test ─────────────────────────────────────────────────────

/// What a parent is launched under beyond its name and grant.
struct ParentConfig {
    task_acceptance: TaskAcceptance,
    after_task: AfterTask,
    /// `inference.max_turns`, or the default when `None`.
    max_turns: Option<u32>,
    /// A `task.md` written into the project before launch, which the session runs as its first
    /// task.
    task_md: Option<String>,
}

impl Default for ParentConfig {
    /// `queue` + `sleep`, so a case can submit task after task to one parent.
    fn default() -> Self {
        Self {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            max_turns: None,
            task_md: None,
        }
    }
}

impl ParentConfig {
    fn lifecycle_yaml(&self) -> String {
        let acceptance = match self.task_acceptance {
            TaskAcceptance::None => "none",
            TaskAcceptance::Single => "single",
            TaskAcceptance::Queue => "queue",
        };
        let after = match self.after_task {
            AfterTask::Exit => "exit",
            AfterTask::Sleep => "sleep",
        };
        format!("lifecycle:\n  task_acceptance: {acceptance}\n  after_task: {after}\n")
    }
}

struct Parent {
    project: PathBuf,
    session_dir: PathBuf,
    url: String,
    server: QueuedServer,
    /// The conversation every task this parent is given runs under, when the case fixed one.
    /// An A2A message that names no `contextId` gets a freshly minted one per task, which is no
    /// conversation for a later `--resume` to continue.
    context_id: Option<String>,
    handle: Option<thread::JoinHandle<capsule_runtime::LaunchResult>>,
}

impl Parent {
    /// Launch one parent capsule in this process, with its own scripted model.
    fn launch(name: &str, spawn_yaml: &str) -> Self {
        Self::launch_in(TempDir::new().unwrap().keep(), name, spawn_yaml, None, None)
    }

    /// The same launch in a named project directory, optionally under a fixed context id and
    /// continuing an earlier session of the same capsule.
    fn launch_in(
        project: PathBuf,
        name: &str,
        spawn_yaml: &str,
        context_id: Option<String>,
        resume: Option<capsule_runtime::ResumeRequest>,
    ) -> Self {
        Self::launch_configured(
            project,
            name,
            spawn_yaml,
            context_id,
            resume,
            ParentConfig::default(),
        )
    }

    /// The same launch under `config`.
    fn launch_configured(
        project: PathBuf,
        name: &str,
        spawn_yaml: &str,
        context_id: Option<String>,
        resume: Option<capsule_runtime::ResumeRequest>,
        config: ParentConfig,
    ) -> Self {
        let suite = suite();
        let server = QueuedServer::start();

        let max_turns = config
            .max_turns
            .map(|turns| format!("  max_turns: {turns}\n"))
            .unwrap_or_default();
        let manifest_body = format!(
            "name: {name}\nversion: {VERSION}\n\
             artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             \x20   gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             capabilities:\n  network:\n    allow: [127.0.0.1]\n  \
             env:\n    allow: [{PROVIDER_KEY_VAR}]\n{spawn_yaml}\
             {lifecycle}\
             inference:\n  transport: http\n  model: test-model\n{max_turns}  \
             driver:\n    artifact: {DRIVER}\n",
            endpoint = server.endpoint,
            lifecycle = config.lifecycle_yaml(),
        );
        let manifest_path = project.join("murmur.yaml");
        std::fs::write(&manifest_path, manifest_body).unwrap();
        let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();

        let fixed_context = context_id.clone();
        let staged = stage_session(
            Arc::new(LocalRegistry::new(&suite.registry)),
            StageRequest {
                credentials_file: None,
                // An empty context is a per-message fact, not a launch-scoped one: `--context`
                // refuses one, while an A2A `contextId` of `""` is taken as the conversation. So
                // an empty id here means "stamp it on every message" and "fix none at launch".
                context_id: context_id.filter(|id| !id.is_empty()),
                resume,
                lifecycle: Some(LifecycleConfig {
                    task_acceptance: config.task_acceptance.clone(),
                    after_task: config.after_task.clone(),
                    queue_depth: 4,
                    input_timeout_secs: None,
                    ..Default::default()
                }),
                ..stage_request(&project, &runtime_manifest)
            },
        )
        .expect("staging should succeed");
        let session_dir = staged.workdir.clone();
        if let Some(task) = &config.task_md {
            std::fs::write(staged.accessible_workdir.join("task.md"), task).unwrap();
        }

        let (url_tx, url_rx) = mpsc::channel::<String>();
        let handle = thread::spawn(move || {
            launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            })
            .expect("launch should succeed")
        });
        let url = url_rx
            .recv_timeout(common::CAPSULE_URL_WAIT)
            .expect("timed out waiting for the capsule URL");

        Self {
            project,
            session_dir,
            url,
            server,
            context_id: fixed_context,
            handle: Some(handle),
        }
    }

    /// This launch's own session id — the name of the directory staging composed for it.
    fn session_id(&self) -> String {
        self.session_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.session_dir.join("trace.jsonl")).unwrap_or_default()
    }

    /// Every line of the parent's trace, in file order, so a case can assert that one record
    /// reached disk before another rather than that one timestamp is lower.
    fn trace_events(&self) -> Vec<Value> {
        parse_events(&self.trace())
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

    /// The one child directory the parent composed, once it has composed exactly one.
    fn only_child_dir(&self) -> PathBuf {
        let dirs = self.child_dirs();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        dirs.into_iter().next().unwrap()
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
        let mut message = json!({"messageId": message_id, "role": "user",
                                 "parts": [{"text": text}]});
        if let Some(context_id) = &self.context_id {
            message["contextId"] = json!(context_id);
        }
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "message/send",
            "params": {"message": message}
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

    /// One delegation turn: the model calls `delegate-task`, then ends its turn, and the task
    /// continues with the outcome. Returns the tool result text the runtime fed back, with the
    /// untrusted-content fence stripped.
    ///
    /// Returns once the task has ended, which is after the sub-capsule's outcome has been
    /// delivered into it.
    fn delegate(&self, tool_id: &str, capsule: &str, version: &str, task: &str) -> String {
        self.delegate_all(
            &format!("msg-{tool_id}"),
            &[(tool_id, capsule, version, task)],
        )
        .remove(0)
    }

    /// One turn in which the model issues several `delegate-task` calls before ending it, in the
    /// order given, then as many continuation turns as the outcomes take. Returns each call's tool
    /// result text, in that order, once the task has ended.
    fn delegate_all(&self, message_id: &str, calls: &[(&str, &str, &str, &str)]) -> Vec<String> {
        // Outcomes arrive in one continuation or in several, so one answer is scripted per
        // delegation and whatever the task did not take is dropped once it has ended.
        let task_id = self.start_delegations(message_id, calls, calls.len());
        self.await_task(&task_id, Duration::from_secs(300));
        self.server.discard_unused();
        self.tool_results(calls)
    }

    /// Script the delegating turn, `continuations` answers after it, and submit the task. Returns
    /// the task id without waiting for anything.
    fn start_delegations(
        &self,
        message_id: &str,
        calls: &[(&str, &str, &str, &str)],
        continuations: usize,
    ) -> String {
        let blocks: Vec<Value> = calls
            .iter()
            .map(|(tool_id, capsule, version, task)| {
                json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": "delegate-task",
                    "input": {"capsule": capsule, "version": version, "task": task}
                })
            })
            .collect();
        self.server.push(
            json!({
                "id": "msg_tools",
                "type": "message",
                "role": "assistant",
                "model": "test-model",
                "content": blocks,
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })
            .to_string(),
        );
        self.server.push(end_turn_response("delegated"));
        for _ in 0..continuations {
            self.server.push(end_turn_response("noted the outcome"));
        }
        self.submit(message_id, "delegate it")
    }

    /// Each call's tool result text, once the turn after the calls has been requested.
    fn tool_results(&self, calls: &[(&str, &str, &str, &str)]) -> Vec<String> {
        calls
            .iter()
            .map(|(tool_id, ..)| {
                let deadline = Instant::now() + Duration::from_secs(240);
                loop {
                    if let Some(text) = tool_result_text(&self.server.requests(), tool_id) {
                        break text;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "no tool_result for {tool_id}; requests: {:?}",
                        self.server.requests()
                    );
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .collect()
    }

    /// Every distinct continuation the model was handed that carries a delegation outcome, in the
    /// order it first appeared. A request carries the whole conversation, so the same message is
    /// in every request after the one it first appeared in.
    fn outcome_messages(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for request in self.server.requests() {
            for message in request
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if message.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                for text in message_texts(message.get("content")) {
                    if text.contains(OUTCOME_OPENING) && !seen.contains(&text) {
                        seen.push(text);
                    }
                }
            }
        }
        seen
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
        credentials_file: None,
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
                gateway: artifact.gateway.clone(),
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
        forget_session: false,
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
        control: None,
        door_authentication: None,
        spawn_grant: None,
        machine_tokens_per_day: None,
        formation_id: None,
        formation_member: None,
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

/// Every JSON line of one trace file, in file order.
fn parse_events(trace: &str) -> Vec<Value> {
    trace
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|error| panic!("trace line is not JSON ({error}): {line}"))
        })
        .collect()
}

/// The child's own `trace.jsonl`, found from the parent's side exactly as an operator would:
/// the child directory the parent composed, then the session directory beneath it.
fn child_events(child_dir: &Path, child_session_id: &str) -> Vec<Value> {
    let path = child_dir
        .join(".murmur")
        .join(child_session_id)
        .join("trace.jsonl");
    parse_events(
        &std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("the child kept no trace at {} ({error})", path.display())
        }),
    )
}

/// Assert that no outcome became a task of its own: no `completion`-origin `task_start`, and no
/// `task_start` naming a delegation.
fn assert_no_completion_task(parent: &Parent) {
    for start in parent.events("task_start") {
        assert_ne!(start["origin"], "completion", "{start}");
        assert!(start.get("delegation_id").is_none(), "{start}");
    }
}

/// Wait until the parent's trace holds at least `count` lines of `event_type`.
fn wait_for_events(
    parent: &Parent,
    event_type: &str,
    count: usize,
    within: Duration,
) -> Vec<Value> {
    let deadline = Instant::now() + within;
    loop {
        let events = parent.events(event_type);
        if events.len() >= count {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} {event_type} lines; the parent wrote {}",
            events.len()
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// The first user message the parent's model was shown that contains `needle` — the text the
/// agent was actually handed, read from the wire rather than rebuilt.
fn await_model_message(parent: &Parent, needle: &str, within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        for request in parent.server.requests() {
            for message in request
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if message.get("role").and_then(Value::as_str) != Some("user") {
                    continue;
                }
                for text in message_texts(message.get("content")) {
                    if text.contains(needle) {
                        return text;
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the model was never shown a message containing {needle:?}"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// The line the runtime opens every delegation outcome with, and the needle a case that waits for
/// one looks for.
const OUTCOME_OPENING: &str = "[delegate-task] delegation ";

/// Every text an Anthropic message `content` carries, whichever of its two shapes it is in.
fn message_texts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The one event of `event_type` in `events`.
fn only_event<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    let matching: Vec<&Value> = events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect();
    assert_eq!(matching.len(), 1, "{event_type}: {matching:?}");
    matching[0]
}

/// The file position of the one line whose `event_type` is `event_type`.
fn position_of(events: &[Value], event_type: &str) -> usize {
    events
        .iter()
        .position(|event| event["event_type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} line: {events:?}"))
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

const SPAWN_YAML: &str = "  spawn:\n    allow: [worker, worker-two, worker-three, greedy-worker, \
                          mute-worker, deep-worker, thirsty-worker]\n";

// ── Cases ─────────────────────────────────────────────────────────────────────

/// The happy path, and the leak sweep over what it left behind.
///
/// The agent names a capsule, a version and a task, and is handed back a delegation in flight. Its
/// task stays `working` until the sub-capsule's outcome arrives, then continues in the same
/// conversation with it, naming the file the answer is in — on `queue` + `sleep` exactly as on
/// every other lifecycle. Neither workdir, neither trace nor the model's own context holds a token
/// of either kind, and the parent goes on to run its next task.
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
        json!([
            WORKER,
            WORKER_TWO,
            WORKER_THREE,
            GREEDY_WORKER,
            MUTE_WORKER,
            DEEP_WORKER,
            THIRSTY_WORKER
        ]),
        "the enum is this capsule's own spawn.allow"
    );
    // And the description says what the call does: it returns on start, naming the delegation,
    // and the task waits for the outcome once the turn ends.
    let description = declared["description"]
        .as_str()
        .expect("the manifest carries a description")
        .to_string();
    for said in [
        "return as soon as it is running and holding that task",
        "does not wait for the sub-capsule to finish",
        "`delegation_id`",
        "`child_workdir`",
        "this task waits for every sub-capsule it started",
        "`result: none`",
    ] {
        assert!(
            description.contains(said),
            "missing '{said}': {description}"
        );
    }

    let text = parent.delegate("toolu_happy", WORKER, VERSION, "summarise the report");

    // The description on disk is the one the model was sent.
    let offered: Vec<String> = parent
        .server
        .requests()
        .iter()
        .filter_map(|request| request.get("tools").and_then(Value::as_array).cloned())
        .flatten()
        .filter(|tool| tool.get("name").and_then(Value::as_str) == Some("delegate-task"))
        .filter_map(|tool| {
            tool.get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(
        !offered.is_empty(),
        "the model was never offered delegate-task"
    );
    for sent in &offered {
        assert_eq!(
            sent, &description,
            "the provider was sent a different description"
        );
    }
    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));

    // The call returns when the child starts: a delegation in flight, named, with nowhere for an
    // answer to be yet.
    assert_eq!(result["status"], "started", "{result}");
    assert_eq!(result["capsule"], WORKER, "{result}");
    assert_eq!(result["version"], VERSION, "{result}");
    assert_eq!(
        result.get("output"),
        None,
        "a started delegation has no output: {result}"
    );
    assert_eq!(result.get("result_path"), None, "{result}");
    assert!(
        result["child_workdir"]
            .as_str()
            .unwrap_or_default()
            .starts_with(&format!(".murmur/children/{WORKER}-")),
        "{result}"
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
    let child_dir = parent.only_child_dir();
    assert!(
        child_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!("{WORKER}-"))),
        "{child_dir:?}"
    );

    // The parent names the child at launch, not at completion.
    let started = parent.events("delegation_start");
    assert_eq!(started.len(), 1, "{started:?}");
    let started = &started[0];
    let started_id = started["delegation_id"].as_str().unwrap_or_default();
    assert!(
        started_id.starts_with("dlg_")
            && started_id["dlg_".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{started}"
    );
    assert_eq!(started["capsule"], WORKER, "{started}");
    assert_eq!(started["version"], VERSION, "{started}");
    let child_session_id = started["child_session_id"].as_str().unwrap_or_default();
    assert!(!child_session_id.is_empty(), "{started}");
    assert_eq!(started_id, delegation_id, "{started}");
    assert_eq!(
        started["child_session_id"], result["session_id"],
        "{started}"
    );

    // And the child names the parent, in its own `session_start`, under the child directory the
    // parent composed.
    let child_trace = child_events(&child_dir, child_session_id);
    let child_start = only_event(&child_trace, "session_start");
    assert_eq!(child_start["spawned_by"], json!(parent.session_id()));
    assert_eq!(child_start["delegation_id"], json!(delegation_id));

    // The outcome arrived inside the delegating task: the launch, the row's close and the task's
    // one end are on disk in that order, and no outcome became a task of its own.
    let trace = parent.trace_events();
    let task_ends: Vec<usize> = trace
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event_type"] == "task_end")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(task_ends.len(), 1, "{trace:?}");
    assert!(
        position_of(&trace, "delegation_start") < position_of(&trace, "delegation")
            && position_of(&trace, "delegation") < task_ends[0],
        "the task ended before its delegation did: {trace:?}"
    );
    assert_no_completion_task(&parent);

    // What the agent was handed names the delegation and where the answer is, and never the
    // answer itself.
    let handed = await_model_message(&parent, OUTCOME_OPENING, Duration::from_secs(120));
    assert!(
        handed.starts_with(&format!(
            "{OUTCOME_OPENING}{delegation_id} to {WORKER}@{VERSION} ended ok:\n"
        )),
        "{handed}"
    );
    assert!(
        handed.contains(&format!("delegation_id: {delegation_id}")),
        "{handed}"
    );
    assert!(handed.contains("status: ok"), "{handed}");
    assert!(
        handed.contains("/out/result.txt (in that workdir)"),
        "the outcome names the child's result file rather than carrying it: {handed}"
    );
    assert!(
        !handed.contains(WORKER_ANSWER),
        "the child's answer stays in its file: {handed}"
    );

    // The child recorded the same outcome for itself, and the delivery landed.
    let completion: Value = serde_json::from_str(
        &std::fs::read_to_string(child_dir.join("completion.json"))
            .expect("the child records its own outcome"),
    )
    .unwrap();
    assert_eq!(completion["reported_by"], "child", "{completion}");
    assert_eq!(completion["delivered"], json!(true), "{completion}");
    assert_eq!(completion["status"], "ok", "{completion}");

    // The row closes out of that file rather than out of the text.
    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    let event = &events[0];
    assert_eq!(event["outcome"], "ok", "{event}");
    assert_eq!(event["delegation_id"], json!(delegation_id), "{event}");
    assert_eq!(event["capsule"], WORKER, "{event}");
    assert_eq!(event["version"], VERSION, "{event}");
    assert_eq!(event["child_session_id"], result["session_id"], "{event}");
    assert_eq!(event["reason"], Value::Null, "{event}");

    // The relationship is recorded once, and by that name. `parent_id` is the event-tree edge and
    // names an `event_id`, never a session.
    let mut objects: Vec<(PathBuf, Value)> = child_trace
        .iter()
        .map(|event| {
            (
                child_dir
                    .join(".murmur")
                    .join(child_session_id)
                    .join("trace.jsonl"),
                event.clone(),
            )
        })
        .collect();
    for path in files_under(&child_dir).into_iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| name == "completion.json")
    }) {
        let raw = std::fs::read_to_string(&path).unwrap();
        objects.push((path, serde_json::from_str(&raw).unwrap()));
    }
    for (path, object) in objects {
        let named: Vec<&str> = object
            .as_object()
            .map(|fields| {
                fields
                    .keys()
                    .map(String::as_str)
                    .filter(|key| {
                        matches!(
                            *key,
                            "spawned_by"
                                | "parent_session"
                                | "parent_session_id"
                                | "spawner_session"
                                | "formation_id"
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            named.is_empty() || named == ["spawned_by"],
            "{}: {named:?}",
            path.display()
        );
    }

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

    // The child reached its provider with the key its manifest referenced. The parent held the
    // variable and both manifests declared it, so the launcher copied it across a cleared
    // environment and the child resolved `${…}` against what it was handed — a child that merely
    // started would have died at its own manifest load instead.
    let outbound = suite.worker_inference.requests();
    assert!(
        outbound.iter().any(|request| request
            .windows(PROVIDER_KEY_VALUE.len())
            .any(|window| window == PROVIDER_KEY_VALUE.as_bytes())),
        "the sub-capsule's inference endpoint saw {} requests, none carrying the resolved key",
        outbound.len()
    );

    let model_context = serde_json::to_string(&parent.server.requests()).unwrap();
    let mut needles: Vec<String> = vec![CREDENTIAL_PREFIX.to_string(), APPROVAL_PREFIX.to_string()];
    needles.push(PROVIDER_KEY_VALUE.to_string());
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

    // The parent is still taking work: a second task runs to its end.
    parent.server.push(end_turn_response("second task done"));
    let second = parent.submit("msg-second", "and another thing");
    parent.await_task(&second, Duration::from_secs(120));
    assert_eq!(parent.task_state(&second), "completed");
}

/// A child that declares `network.authentication` is driven by its unauthenticated parent with the
/// operator token from its readiness line, and its outcome arrives exactly as a public child's
/// does. No door token reaches the parent's workdir, its trace or its model.
#[test]
fn authenticated_child_is_driven_with_its_operator_token() {
    if common::skip_without_host_support("authenticated_child_is_driven_with_its_operator_token") {
        return;
    }
    let _suite = suite();
    let parent = Parent::launch(PARENT, &format!("  spawn:\n    allow: [{AUTH_WORKER}]\n"));

    let text = parent.delegate("toolu_auth", AUTH_WORKER, VERSION, "summarise the report");
    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));
    assert_eq!(result["status"], "started", "{result}");
    let delegation_id = result["delegation_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    assert_no_completion_task(&parent);
    let handed = await_model_message(&parent, OUTCOME_OPENING, Duration::from_secs(120));
    assert!(
        handed.contains(&format!("delegation_id: {delegation_id}")),
        "{handed}"
    );
    assert!(handed.contains("status: ok"), "{handed}");

    let child_dir = parent.only_child_dir();
    let completion: Value = serde_json::from_str(
        &std::fs::read_to_string(child_dir.join("completion.json"))
            .expect("the child records its own outcome"),
    )
    .unwrap();
    assert_eq!(completion["status"], "ok", "{completion}");
    assert_eq!(completion["delivered"], json!(true), "{completion}");

    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "ok", "{events:?}");
    assert_eq!(events[0]["capsule"], AUTH_WORKER, "{events:?}");

    let model_context = serde_json::to_string(&parent.server.requests()).unwrap();
    assert!(
        !model_context.contains("mdt1."),
        "a door token reached the model"
    );
    assert!(
        !text.contains("mdt1."),
        "a door token reached the tool result"
    );
    for root in [&parent.project, &parent.session_dir] {
        if let Some(path) = find_in_files(root, "mdt1.") {
            panic!("a door token reached {}", path.display());
        }
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
    assert_eq!(events[0]["child_session_id"], Value::Null, "{}", events[0]);
    assert_eq!(events[0]["reason"], json!(text), "{}", events[0]);
    assert!(events[0]["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("capabilities.shell.allow"));
    assert!(events[0]["reason"]
        .as_str()
        .unwrap_or_default()
        .contains("bash"));

    // Nothing launched, so nothing opened a delegation.
    assert!(
        parent.events("delegation_start").is_empty(),
        "a refused delegation launched no child"
    );
}

/// The env axis refuses like every other axis: a sub-capsule naming a host variable its parent
/// does not declare is refused before anything launches, and the sentence names the key and the
/// variable.
#[test]
fn a_child_naming_an_undeclared_variable_is_refused_on_the_env_axis() {
    if common::skip_without_host_support(
        "a_child_naming_an_undeclared_variable_is_refused_on_the_env_axis",
    ) {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    let text = parent.delegate("toolu_thirsty", THIRSTY_WORKER, VERSION, "hand me a key");

    assert!(
        text.contains("capabilities.env.allow"),
        "the refusal names the axis: {text}"
    );
    assert!(
        text.contains(UNGRANTED_VAR),
        "the refusal names the entry: {text}"
    );

    // Refused, so nothing was launched and nothing was composed beneath the parent's workdir.
    assert!(parent.child_dirs().is_empty(), "no child directory exists");
    assert!(
        parent.events("delegation_start").is_empty(),
        "a refused delegation launched no child"
    );

    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "refused", "{}", events[0]);
    assert_eq!(events[0]["reason"], json!(text), "{}", events[0]);
}

/// A bound the daemon refuses on reaches the parent's trace as the daemon's own sentence, unedited.
///
/// The proxy answers this one spawn itself, with the refusal type `mur-roost` formats its own
/// bounds from, so the wording under test is the daemon's rather than a hand-copied string. That
/// the daemon emits it for a real depth exhaustion is `mur-roost`'s own suite; what this proves is
/// the joining fact — whatever it refuses with arrives at the parent unaltered.
#[test]
fn a_bound_refusal_reaches_the_parents_trace_unaltered() {
    if common::skip_without_host_support("a_bound_refusal_reaches_the_parents_trace_unaltered") {
        return;
    }
    let sentence = mur_roost::bounds::BoundRefusal::DepthExhausted {
        max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
    }
    .to_string();
    suite().roost.refuse_spawns_of(DEEP_WORKER, &sentence);

    let parent = Parent::launch(PARENT, SPAWN_YAML);
    let text = parent.delegate("toolu_bound", DEEP_WORKER, VERSION, "go one deeper");

    assert_eq!(text, sentence, "the model was given the daemon's sentence");
    assert!(text.contains("--max-depth"), "{text}");

    let events = parent.events("delegation");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["outcome"], "refused", "{}", events[0]);
    assert_eq!(events[0]["delegation_id"], Value::Null, "{}", events[0]);
    assert_eq!(events[0]["reason"], json!(sentence), "{}", events[0]);
    assert!(parent.events("delegation_start").is_empty());
    assert!(parent.child_dirs().is_empty(), "no child directory exists");
}

/// A started child that never ends is stopped at the delegation deadline, and the delegating task
/// continues with that.
///
/// The call itself returns promptly. The task then waits — `working`, spending nothing — until the
/// deadline ends the child, and continues with the one thing an agent can act on: a `terminated`
/// outcome naming the bound.
#[test]
fn a_wedged_started_child_is_ended_at_the_deadline() {
    if common::skip_without_host_support("a_wedged_started_child_is_ended_at_the_deadline") {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    let calls = [("toolu_mute", MUTE_WORKER, VERSION, "never answer this")];
    let started_at = Instant::now();
    let task_id = parent.start_delegations("msg-mute", &calls, 1);
    let text = parent.tool_results(&calls).remove(0);
    let call_took = started_at.elapsed();

    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));
    assert_eq!(result["status"], "started", "{result}");
    let delegation_id = result["delegation_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(delegation_id.starts_with("dlg_"), "{result}");
    let child_session_id = result["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let child_dir = parent.only_child_dir();

    // The launch is on disk, nothing has closed it, and the task is waiting on it.
    let trace = parent.trace_events();
    let launch = only_event(&trace, "delegation_start");
    assert_eq!(launch["delegation_id"], json!(delegation_id), "{launch}");
    assert_eq!(launch["capsule"], MUTE_WORKER, "{launch}");
    assert!(
        parent.events("delegation").is_empty(),
        "the call returned in {call_took:?} with the delegation already closed"
    );
    thread::sleep(Duration::from_secs(2));
    assert_eq!(
        parent.task_state(&task_id),
        "working",
        "the task does not end while its delegation is outstanding"
    );

    // At the deadline the task continues, and then ends.
    parent.await_task(&task_id, Duration::from_secs(TIMEOUT_SECS + 240));
    let waited = started_at.elapsed();
    assert_eq!(parent.task_state(&task_id), "completed");
    assert!(
        waited >= Duration::from_secs(TIMEOUT_SECS),
        "the task ended after {waited:?}, before the {TIMEOUT_SECS}s deadline"
    );
    assert_no_completion_task(&parent);

    let handed = await_model_message(&parent, OUTCOME_OPENING, Duration::from_secs(120));
    assert!(
        handed.starts_with(&format!(
            "{OUTCOME_OPENING}{delegation_id} to {MUTE_WORKER}@{VERSION} ended terminated:\n"
        )),
        "{handed}"
    );
    assert!(
        handed.contains(&format!("session_id: {child_session_id}")),
        "{handed}"
    );
    assert!(handed.contains("status: terminated"), "{handed}");
    assert!(
        handed.contains(&format!("{TIMEOUT_SECS}s")) && handed.contains("delegation deadline"),
        "the detail names the deadline in seconds: {handed}"
    );

    // The child's own directory records the same ending, delivered — the watcher wrote it, and
    // posted it, because this ending was the runtime's own doing rather than the task's.
    let completion: Value = serde_json::from_str(
        &std::fs::read_to_string(child_dir.join("completion.json"))
            .expect("the ended child's outcome is recorded in its own directory"),
    )
    .unwrap();
    assert_eq!(completion["reported_by"], "launcher", "{completion}");
    assert_eq!(completion["delivered"], json!(true), "{completion}");
    assert_eq!(completion["status"], "terminated", "{completion}");
    assert_eq!(
        completion["delegation_id"],
        json!(delegation_id),
        "{completion}"
    );

    // The row closes with the same word, out of that file, before the task's end.
    let trace = parent.trace_events();
    let ended = only_event(&trace, "delegation");
    assert_eq!(ended["outcome"], "terminated", "{ended}");
    assert_eq!(ended["delegation_id"], json!(delegation_id));
    assert!(
        position_of(&trace, "delegation_start") < position_of(&trace, "delegation")
            && position_of(&trace, "delegation") < position_of(&trace, "task_end"),
        "{trace:?}"
    );
    assert!(
        !a_process_is_running_under(&child_dir),
        "the ended child is still running"
    );

    // The parent is still its own capsule afterwards: it answers the next turn.
    parent.server.push(end_turn_response("still here"));
    let task_id = parent.submit("msg-after-deadline", "are you there");
    parent.await_task(&task_id, Duration::from_secs(120));
    assert_eq!(parent.task_state(&task_id), "completed");
}

/// Three delegations in one turn, and the turn ends before any of them does.
///
/// This is what returning on start buys: the parent issues three calls in one turn, ends it, and
/// its task collects all three outcomes before it ends — each named exactly once across the
/// continuations, none a task of its own.
#[test]
fn three_delegations_leave_one_turn_and_every_outcome_arrives_before_the_task_ends() {
    if common::skip_without_host_support(
        "three_delegations_leave_one_turn_and_every_outcome_arrives_before_the_task_ends",
    ) {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    let results: Vec<Value> = parent
        .delegate_all(
            "msg-fanout",
            &[
                ("toolu_fan_1", WORKER, VERSION, "first"),
                ("toolu_fan_2", WORKER_TWO, VERSION, "second"),
                ("toolu_fan_3", WORKER_THREE, VERSION, "third"),
            ],
        )
        .iter()
        .map(|text| {
            serde_json::from_str(text)
                .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"))
        })
        .collect();

    let mut ids = Vec::new();
    for (result, capsule) in results.iter().zip([WORKER, WORKER_TWO, WORKER_THREE]) {
        assert_eq!(result["status"], "started", "{result}");
        assert_eq!(result["capsule"], capsule, "{result}");
        assert!(
            result["child_workdir"]
                .as_str()
                .unwrap_or_default()
                .starts_with(&format!(".murmur/children/{capsule}-")),
            "{result}"
        );
        ids.push(
            result["delegation_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
    }
    let mut distinct = ids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        3,
        "three delegations, three ids: {results:?}"
    );

    // Three launches, three directories, and every launch on disk before the task ended.
    let trace = parent.trace_events();
    let launches: Vec<&Value> = trace
        .iter()
        .filter(|event| event["event_type"] == "delegation_start")
        .collect();
    assert_eq!(launches.len(), 3, "{launches:?}");
    let workdirs: HashSet<&str> = launches
        .iter()
        .map(|event| event["child_workdir"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(workdirs.len(), 3, "{launches:?}");
    assert_eq!(parent.child_dirs().len(), 3, "{:?}", parent.child_dirs());
    assert!(
        position_of(&trace, "delegation_start") < position_of(&trace, "task_end"),
        "the launches are on disk before the task ends"
    );

    // Three terminal lines, each closing the start it belongs to, all before the task's one end.
    let ended: Vec<&Value> = trace
        .iter()
        .filter(|event| event["event_type"] == "delegation")
        .collect();
    let mut closed: Vec<String> = ended
        .iter()
        .map(|event| {
            assert_eq!(event["outcome"], "ok", "{event}");
            event["delegation_id"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    closed.sort();
    assert_eq!(closed, distinct, "{ended:?}");
    let task_end = position_of(&trace, "task_end");
    assert_eq!(
        trace
            .iter()
            .filter(|event| event["event_type"] == "task_end")
            .count(),
        1,
        "{trace:?}"
    );
    for (index, event) in trace.iter().enumerate() {
        if event["event_type"] == "delegation" {
            assert!(index < task_end, "{event} after the task ended");
        }
    }

    // Every outcome reached the model exactly once, across however many continuations it took.
    let continuations = parent.outcome_messages();
    assert!(
        !continuations.is_empty() && continuations.len() <= 3,
        "{continuations:?}"
    );
    for id in &distinct {
        let named = continuations
            .iter()
            .map(|text| text.matches(&format!("{OUTCOME_OPENING}{id} ")).count())
            .sum::<usize>();
        assert_eq!(named, 1, "{id} in {continuations:?}");
    }
    assert_no_completion_task(&parent);
}

/// The parent stays reachable while its task waits on a child.
///
/// The turn ends once the sub-capsule holds its task, and the task waits for the outcome off the
/// thread that serves the door: the listener answers throughout, and the task is `working` until
/// the deadline ends a child that never answers.
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
    parent.server.push(end_turn_response("noted the outcome"));
    let task_id = parent.submit("msg-inflight", "delegate it");

    // Wait for the delegation to actually be under way — the child has to be launched before
    // anything about the parent's availability during it means anything.
    let deadline = Instant::now() + Duration::from_secs(240);
    while parent.child_dirs().is_empty() {
        assert!(
            Instant::now() < deadline,
            "the child was never launched; task state: {}",
            parent.task_state(&task_id)
        );
        thread::sleep(Duration::from_millis(200));
    }

    // The listener answers, repeatedly, throughout.
    for _ in 0..5 {
        assert_eq!(
            agent_card_status(&parent.url),
            200,
            "the parent's card must answer while its delegation runs"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(parent.task_state(&task_id), "working");

    // This child answers nothing, ever, so the task ends only once the deadline has ended it.
    parent.await_task(&task_id, Duration::from_secs(TIMEOUT_SECS + 240));
    assert_eq!(parent.task_state(&task_id), "completed");
    let ended = parent.events("delegation");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["outcome"], "terminated", "{}", ended[0]);
}

/// A cancel while the task waits on a delegation names the child and ends it.
///
/// `tasks/cancel` answers at once, and its residue carries the `dlg_` id the `delegation_start`
/// opened, snapshotted before the task ends the child. The task then ends the child itself: the
/// trace says so after the cancel and before the task's end, the child's own record says its
/// parent ended it, and the process is gone. The parent goes on taking work.
#[test]
fn a_cancel_mid_delegation_names_the_child_and_ends_it() {
    if common::skip_without_host_support("a_cancel_mid_delegation_names_the_child_and_ends_it") {
        return;
    }
    let parent = Parent::launch(PARENT, SPAWN_YAML);

    // One turn that delegates and one that ends, so the task is left waiting on a child that
    // never answers.
    let calls = [("toolu_cancel", MUTE_WORKER, VERSION, "hold the line")];
    let task_id = parent.start_delegations("msg-cancel", &calls, 0);
    parent.tool_results(&calls);
    let deadline = Instant::now() + Duration::from_secs(60);
    while parent.server.requests().len() < 2 {
        assert!(Instant::now() < deadline, "the turn never ended");
        thread::sleep(Duration::from_millis(50));
    }
    // The turn has been answered; give the task a moment to reach its wait.
    thread::sleep(Duration::from_secs(1));
    assert_eq!(parent.task_state(&task_id), "working");
    let launch = only_event(&parent.trace_events(), "delegation_start").clone();
    let delegation_id = launch["delegation_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(delegation_id.starts_with("dlg_"), "{launch}");
    let child_dir = parent.only_child_dir();
    assert!(a_process_is_running_under(&child_dir));

    let started_at = Instant::now();
    let response = post_json(
        &parent.url,
        &json!({
            "jsonrpc": "2.0", "id": 9, "method": "tasks/cancel", "params": {"id": task_id}
        })
        .to_string(),
    );
    let took = started_at.elapsed();

    assert_eq!(
        response["result"]["status"]["state"], "canceled",
        "{response}"
    );
    assert!(
        took < Duration::from_secs(5),
        "the cancel queued behind the child ({took:?}); it must not wait on a delegation"
    );

    let artifacts = response["result"]["artifacts"]
        .as_array()
        .unwrap_or_else(|| panic!("a cancel with a live delegation carries residue: {response}"));
    let items: Vec<Value> = artifacts
        .iter()
        .filter(|artifact| artifact["name"] == "residue")
        .filter_map(|artifact| artifact["parts"].as_array())
        .flatten()
        .filter_map(|part| serde_json::from_str::<Value>(part["text"].as_str()?).ok())
        .collect();
    let delegation = items
        .iter()
        .find(|item| item["kind"] == "delegation")
        .unwrap_or_else(|| panic!("no delegation residue in {items:?}"));
    assert_eq!(
        delegation["delegation_id"],
        json!(delegation_id),
        "{delegation}"
    );
    assert_eq!(delegation["capsule"], MUTE_WORKER, "{delegation}");

    // The child is ended by the task, not left to run out its deadline.
    let gone = Instant::now() + Duration::from_secs(30);
    while a_process_is_running_under(&child_dir) {
        assert!(
            Instant::now() < gone,
            "the delegated child is still running 30s after its task was cancelled"
        );
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "[measure] tasks/cancel to child gone: {} ms",
        started_at.elapsed().as_millis()
    );

    // The record: cancelled while waiting on the delegation, the delegation ended because of it,
    // and only then the task's end.
    wait_for_events(&parent, "task_end", 1, Duration::from_secs(60));
    let trace = parent.trace_events();
    let canceled = position_of(&trace, "task_canceled");
    assert_eq!(
        trace[canceled]["phase"], "delegation",
        "{}",
        trace[canceled]
    );
    assert!(
        trace[canceled]["delegation_ids"]
            .as_array()
            .is_some_and(|ids| ids.contains(&json!(delegation_id))),
        "{}",
        trace[canceled]
    );
    let ended = position_of(&trace, "delegation");
    assert_eq!(trace[ended]["outcome"], "terminated", "{}", trace[ended]);
    assert_eq!(
        trace[ended]["reason"], "the delegating task was cancelled",
        "{}",
        trace[ended]
    );
    assert!(
        canceled < ended && ended < position_of(&trace, "task_end"),
        "{trace:?}"
    );

    let completion: Value = serde_json::from_str(
        &std::fs::read_to_string(child_dir.join("completion.json"))
            .expect("the ended child's outcome is recorded"),
    )
    .unwrap();
    assert_eq!(completion["status"], "terminated", "{completion}");
    assert_eq!(completion["reported_by"], "launcher", "{completion}");
    assert_eq!(completion["delivered"], json!(false), "{completion}");
    assert_eq!(
        completion["detail"], "the parent ended this delegation",
        "{completion}"
    );

    // And the parent is still answering for itself, and taking work.
    assert_eq!(agent_card_status(&parent.url), 200);
    parent.server.push(end_turn_response("after the cancel"));
    let next = parent.submit("msg-after-cancel", "carry on");
    parent.await_task(&next, Duration::from_secs(120));
    assert_eq!(parent.task_state(&next), "completed");
}

/// A capsule that accepts no tasks over its door still hears from the sub-capsule its `task.md`
/// delegated to: a completion is handed to the waiting task ahead of the door's method rules, and
/// the session then ends as a `none` session does.
#[test]
fn a_capsule_that_accepts_no_tasks_still_hears_from_its_sub_capsule() {
    if common::skip_without_host_support(
        "a_capsule_that_accepts_no_tasks_still_hears_from_its_sub_capsule",
    ) {
        return;
    }
    let mut parent = Parent::launch_configured(
        TempDir::new().unwrap().keep(),
        PARENT,
        SPAWN_YAML,
        None,
        None,
        ParentConfig {
            task_acceptance: TaskAcceptance::None,
            after_task: AfterTask::Exit,
            task_md: Some("delegate it".to_string()),
            ..ParentConfig::default()
        },
    );
    parent.server.push(tool_use_response(
        "toolu_none",
        "delegate-task",
        json!({"capsule": WORKER, "version": VERSION, "task": "summarise the report"}),
    ));
    parent.server.push(end_turn_response("delegated"));
    parent.server.push(end_turn_response("noted the outcome"));

    // The launch thread expects `launch_session` to succeed, so a join is the session ending
    // without an error.
    parent
        .handle
        .take()
        .expect("the launch thread")
        .join()
        .expect("the session ends normally");
    let ends = parent.events("session_end");
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0]["exit_status"], "ok", "{}", ends[0]);

    let starts = parent.events("task_start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0]["source"], "task_md", "{}", starts[0]);
    assert_no_completion_task(&parent);
    let ended = parent.events("delegation");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["outcome"], "ok", "{}", ended[0]);
    let delegation_id = ended[0]["delegation_id"].as_str().unwrap().to_string();
    let handed = await_model_message(&parent, OUTCOME_OPENING, Duration::from_secs(5));
    assert!(
        handed.contains(&format!("delegation_id: {delegation_id}")),
        "{handed}"
    );
    let completion: Value = serde_json::from_str(
        &std::fs::read_to_string(parent.only_child_dir().join("completion.json"))
            .expect("the child records its own outcome"),
    )
    .unwrap();
    assert_eq!(completion["delivered"], json!(true), "{completion}");
}

/// A task whose delegating turn was its last allowed turn has nothing left to read an outcome
/// with, so it does not wait: it ends the sub-capsule it started, says why, and ends.
#[test]
fn a_task_with_no_turn_left_ends_its_sub_capsules() {
    if common::skip_without_host_support("a_task_with_no_turn_left_ends_its_sub_capsules") {
        return;
    }
    let parent = Parent::launch_configured(
        TempDir::new().unwrap().keep(),
        PARENT,
        SPAWN_YAML,
        None,
        None,
        ParentConfig {
            max_turns: Some(2),
            ..ParentConfig::default()
        },
    );
    let calls = [("toolu_last", MUTE_WORKER, VERSION, "hold the line")];
    let started_at = Instant::now();
    let task_id = parent.start_delegations("msg-last", &calls, 0);
    parent.await_task(&task_id, Duration::from_secs(240));
    let took = started_at.elapsed();
    assert_eq!(parent.task_state(&task_id), "completed");
    assert!(
        took < Duration::from_secs(TIMEOUT_SECS),
        "the task waited {took:?} for an outcome it had no turn to read"
    );

    let ended = parent.events("delegation");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["outcome"], "terminated", "{}", ended[0]);
    assert_eq!(
        ended[0]["reason"], "the delegating task had no inference turn left to read this outcome",
        "{}",
        ended[0]
    );
    let child_dir = parent.only_child_dir();
    let gone = Instant::now() + Duration::from_secs(30);
    while a_process_is_running_under(&child_dir) {
        assert!(Instant::now() < gone, "the child is still running");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        parent.server.requests().len(),
        2,
        "{:?}",
        parent.server.requests()
    );
}

/// Whether any process on this host was started against `workdir` — the child's launch names it
/// on its own command line, and the directory is unique per delegation.
fn a_process_is_running_under(workdir: &Path) -> bool {
    std::process::Command::new("ps")
        .args(["-eo", "args"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&workdir.display().to_string()))
        .unwrap_or(false)
}

/// Lineage survives a resume of the parent, with no field added and nothing rewritten.
///
/// The child's `spawned_by` names the session that spawned it and is never revisited; the resumed
/// session's `resumed_from` names that same session, so the child is a child of the resumed parent
/// by one hop through facts that were already recorded.
#[test]
fn lineage_survives_a_resume_of_the_parent() {
    if common::skip_without_host_support("lineage_survives_a_resume_of_the_parent") {
        return;
    }
    // One project directory and one context for both launches: `--resume` continues a
    // conversation, and the record it continues has to be the one the first launch wrote.
    let project = TempDir::new().unwrap().keep();
    let context_id = format!("ctx-lineage-{}", std::process::id());

    let first = Parent::launch_in(
        project.clone(),
        PARENT,
        SPAWN_YAML,
        Some(context_id.clone()),
        None,
    );
    let text = first.delegate("toolu_resumed", WORKER, VERSION, "answer once");
    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));
    let child_session_id = result["session_id"].as_str().unwrap().to_string();
    // The child's own `session_start` is what this case reads, so the delegation has run its
    // course — `delegate` returns once the task has its outcome — before the parent it names
    // goes away.
    assert_eq!(first.events("delegation").len(), 1);
    let first_session_id = first.session_id();
    let child_dir = first.only_child_dir();
    drop(first);

    let second = Parent::launch_in(
        project.clone(),
        PARENT,
        SPAWN_YAML,
        Some(context_id.clone()),
        Some(capsule_runtime::ResumeRequest {
            from_session: first_session_id.clone(),
            mode: capsule_runtime::ResumeMode::Full,
        }),
    );
    // The session frame is written by the task loop rather than by the bind, so the resumed
    // session has to run a turn before its `session_start` is on disk.
    second.server.push(end_turn_response("resumed"));
    let task_id = second.submit("msg-resumed", "carry on");
    second.await_task(&task_id, Duration::from_secs(300));

    let resumed_start = only_event(&second.trace_events(), "session_start").clone();
    assert_eq!(resumed_start["resumed_from"], json!(first_session_id));
    assert_eq!(
        resumed_start.get("spawned_by"),
        None,
        "an operator's resume is not a delegation: {resumed_start}"
    );

    let child_start = only_event(
        &child_events(&child_dir, &child_session_id),
        "session_start",
    )
    .clone();
    assert_eq!(
        child_start["spawned_by"],
        json!(first_session_id),
        "the child still names the session that spawned it"
    );
}

/// A delegation made from no conversation is refused before anything is launched.
///
/// An A2A client may send an empty `contextId`, which is taken as the conversation rather than
/// replaced by a minted one. There is then no conversation for a completion to run under, and a
/// delegation whose outcome has nowhere to arrive is a way to lose work — so it is refused rather
/// than started. No daemon request, no child directory, no launch line.
#[test]
fn a_delegation_with_no_conversation_is_refused_rather_than_started() {
    if common::skip_without_host_support(
        "a_delegation_with_no_conversation_is_refused_rather_than_started",
    ) {
        return;
    }
    let parent = Parent::launch_in(
        TempDir::new().unwrap().keep(),
        PARENT,
        SPAWN_YAML,
        Some(String::new()),
        None,
    );
    let text = parent.delegate("toolu_unnamed", WORKER, VERSION, "answer once");
    let result: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the tool result is JSON ({error}): {text}"));

    assert_eq!(result["status"], "failed", "{text}");
    assert!(
        result["output"]
            .as_str()
            .unwrap_or_default()
            .contains("nowhere to be reported"),
        "the sentence says why: {text}"
    );
    assert_eq!(result.get("child_workdir"), None, "{text}");
    assert!(parent.child_dirs().is_empty(), "nothing was launched");

    // One terminal line, closing nothing because nothing was opened, and naming no delegation.
    assert!(
        parent.events("delegation_start").is_empty(),
        "an unstarted delegation opens no launch line"
    );
    let ended = parent.events("delegation");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0]["outcome"], "failed", "{}", ended[0]);
    assert_eq!(ended[0]["delegation_id"], Value::Null, "{}", ended[0]);
}
