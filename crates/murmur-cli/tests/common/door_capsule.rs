//! Capsules that declare `network.authentication`, and a raw HTTP client that shows everything
//! the door answers: status, headers and body.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

pub const DRIVER_NAME: &str = "murmur-driver-anthropic";
pub const DRIVER_VERSION: &str = "0.1.4";

/// `network.authentication` with a `watcher` that may read tasks and watch, and a `reader` that
/// may read the operator resource plane.
pub const AUTHENTICATION_YAML: &str = "network:\n  authentication:\n    scheme: bearer\n    \
     credentials:\n      watcher:\n        scopes: [tasks/get, stream/watch]\n      \
     reader:\n        scopes: [resources/files]\n";

/// Stages the capsule `manifest` describes from `home`'s store, in process, declaring what its
/// `network.authentication` block declares — as the formation member `member`, or as none.
pub fn stage_door(
    home: &TempDir,
    manifest: &Path,
    member: Option<Arc<capsule_runtime::FormationMember>>,
) -> capsule_runtime::StagedSession {
    let runtime_manifest = murmur_artifact::load_runtime_manifest(manifest).unwrap();
    let artifacts = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| capsule_runtime::ArtifactRequest {
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
    capsule_runtime::stage_session(
        Arc::new(murmur_artifact::LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        )),
        capsule_runtime::StageRequest {
            credentials_file: None,
            manifest_dir: manifest.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts,
            allowlisted_tools: Default::default(),
            lock_expectations: None,
            capability_policy: capsule_runtime::capability_policy_from_runtime_manifest(
                &runtime_manifest,
            ),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: None,
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: runtime_manifest.lifecycle.clone(),
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: runtime_manifest.exports.clone(),
            control: None,
            door_authentication: runtime_manifest
                .network
                .as_ref()
                .and_then(|network| network.authentication.clone()),
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: member.as_ref().map(|member| member.formation_id().clone()),
            formation_member: member,
        },
    )
    .unwrap()
}

/// Launches `staged` on a thread of its own and returns its `host:port`. A queue+sleep capsule
/// never exits, so the thread is left behind.
pub fn launch_door(staged: capsule_runtime::StagedSession) -> String {
    let (url_tx, url_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        capsule_runtime::launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    url_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("timed out waiting for the door")
}

/// Every event in the session's `trace.jsonl` so far.
pub fn trace_events(workdir: &Path) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The `Authorization` header value that presents `token`.
pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// An anthropic `end_turn` reply numbered `n`, saying `text`.
pub fn end_turn(n: usize, text: &str) -> String {
    json!({
        "id": format!("msg_{n}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A scratch `$HOME` with the fixture inference driver published into it.
pub fn driver_home() -> TempDir {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver = super::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &super::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    super::publish_local(&home, &driver).success();
    home
}

/// A project whose `murmur.yaml` is an agent capsule named `name` against `endpoint`, with
/// `capabilities_extra` spliced into its capability block and `extra` appended at top level.
pub fn agent_project(endpoint: &str, name: &str, capabilities_extra: &str, extra: &str) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n{capabilities_extra}\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
             {extra}"
        ),
    )
    .unwrap();
    project
}

/// The lifecycle of a capsule that waits at its door and never exits on its own.
pub const QUEUE_SLEEP_YAML: &str =
    "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n";

// ── A capsule running as its own `mur run` process ────────────────────────────

/// A `mur run` child: its readiness output, and everything it writes to stderr.
pub struct MurRun {
    pub child: Child,
    /// The `--json` readiness line, or `{"url": …, "tokens": {…}}` read off the human lines.
    pub startup: Value,
    /// Every stdout line up to and including the readiness output.
    pub stdout_lines: Vec<String>,
    stderr: Arc<Mutex<String>>,
}

impl MurRun {
    /// `mur run --manifest <manifest> [--json] <args>` under `home`, blocking until the door is
    /// announced.
    pub fn start(
        home: &Path,
        manifest: &Path,
        json: bool,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Self {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(["run", "--manifest"])
            .arg(manifest)
            .args(args)
            .current_dir(manifest.parent().unwrap())
            .env("HOME", home)
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if json {
            command.arg("--json");
        }
        let mut child = command.spawn().expect("mur run should start");

        let (line_tx, line_rx) = mpsc::channel::<String>();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let child_stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(child_stderr).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });

        let deadline = Instant::now() + Duration::from_secs(120);
        let mut stdout_lines = Vec::new();
        let startup = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Ok(line) = line_rx.recv_timeout(remaining) else {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "timed out waiting for the door to be announced; stdout so far: {stdout_lines:?}\nstderr: {}",
                    stderr.lock().unwrap()
                );
            };
            stdout_lines.push(line.clone());
            if json {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.get("url").is_some() {
                        break value;
                    }
                }
            } else if line.starts_with("session: ") {
                break human_startup(&stdout_lines);
            }
        };
        MurRun {
            child,
            startup,
            stdout_lines,
            stderr,
        }
    }

    pub fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    pub fn token(&self, name: &str) -> String {
        self.startup["tokens"][name]
            .as_str()
            .unwrap_or_else(|| panic!("no {name} token in {}", self.startup))
            .to_string()
    }

    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for MurRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The url and tokens a human-mode `mur run` printed.
fn human_startup(lines: &[String]) -> Value {
    let mut url = Value::Null;
    let mut tokens = serde_json::Map::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("murmur: url ") {
            url = Value::from(rest.trim());
        } else if let Some(rest) = line.strip_prefix("murmur: token ") {
            let (name, token) = rest.split_once(' ').expect("murmur: token <name> <token>");
            tokens.insert(name.to_string(), Value::from(token));
        }
    }
    json!({"url": url, "tokens": tokens})
}

// ── Raw HTTP ──────────────────────────────────────────────────────────────────

/// One HTTP response: status, headers with lowercased names, body.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("the body is not JSON ({e}): {:?}", self.body))
    }
}

/// Sends one request with `headers` and reads the response head, then the body: up to
/// `content-length` when given, otherwise until the connection closes or goes quiet for a second.
pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Response {
    let stream = TcpStream::connect(addr).expect("should connect to the door");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    (&stream).write_all(head.as_bytes()).unwrap();
    (&stream).write_all(body.as_bytes()).unwrap();
    (&stream).flush().unwrap();

    let mut reader = BufReader::new(&stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status in {status_line:?}"));
    let mut response_headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            response_headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length = response_headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let mut body_bytes = Vec::new();
    match length {
        Some(length) => {
            body_bytes.resize(length, 0);
            reader.read_exact(&mut body_bytes).unwrap();
        }
        None => {
            stream.set_read_timeout(Some(Duration::from_secs(1))).ok();
            let _ = reader.read_to_end(&mut body_bytes);
        }
    }
    Response {
        status,
        headers: response_headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    }
}

/// One JSON-RPC call on `POST /`, presenting `token` as a bearer token when given.
pub fn rpc(addr: &str, token: Option<&str>, method: &str, params: Value) -> Response {
    let authorization = token.map(|token| format!("Bearer {token}"));
    let headers: Vec<(&str, &str)> = authorization
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .collect();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    request(addr, "POST", "/", &headers, &body)
}

/// `message/send` params carrying `text`.
pub fn message(message_id: &str, text: &str) -> Value {
    json!({"message": {"messageId": message_id, "role": "user", "parts": [{"text": text}]}})
}

/// Polls `tasks/get` under `token` until the task reaches `completed`, and returns it.
pub fn wait_completed(addr: &str, token: Option<&str>, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let response = rpc(addr, token, "tasks/get", json!({"id": task_id}));
        assert_eq!(response.status, 200, "{response:?}");
        let task = response.json();
        if task["result"]["status"]["state"] == "completed" {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "the task never completed: {task}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Every file beneath `root` whose bytes contain `needle`.
pub fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                if let Ok(bytes) = fs::read(&path) {
                    if String::from_utf8_lossy(&bytes).contains(needle) {
                        found.push(path);
                    }
                }
            }
        }
    }
    found
}

/// Blocks until `server` has seen `count` requests: a task is then in flight.
pub fn wait_for_requests(server: &super::ScriptedServer, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while server.requests().len() < count {
        assert!(
            Instant::now() < deadline,
            "the provider never saw request {count}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// An authenticated queue+sleep capsule run as its own process under `home`, with one task
/// submitted by the operator and held in flight by a provider that answers after 20 seconds.
/// Returns the capsule, its provider, and the task id.
pub fn authenticated_capsule_with_a_task_in_flight(
    home: &TempDir,
    name: &str,
) -> (MurRun, super::ScriptedServer, TempDir, String) {
    let server =
        super::ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(20));
    let project = agent_project(
        &server.endpoint,
        name,
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let run = MurRun::start(
        home.path(),
        &project.path().join("murmur.yaml"),
        true,
        &[],
        &[],
    );
    let sent = rpc(
        &run.url(),
        Some(&run.token("operator")),
        "message/send",
        message("m-held", "hold"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
    wait_for_requests(&server, 1);
    (run, server, project, task_id)
}

/// `mur <args>` under `home` with `env`, and no door token inherited from this process.
pub fn mur(home: &Path, env: &[(&str, &str)]) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
        .envs(env.iter().copied());
    command
}
