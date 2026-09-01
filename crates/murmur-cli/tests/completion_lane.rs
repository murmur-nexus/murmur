//! Where a completion sits in a real capsule's queue, and what a capsule that never delegates
//! does about any of it.
//!
//! Every case runs a real `launch_session` or a real `mur run`, with a scripted inference
//! endpoint and a real queue. Ordering is read out of the capsule's own `trace.jsonl` — an
//! assertion about `LaneQueue` in isolation would say nothing about the queue an operator has.

#[path = "common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use capsule_runtime::delegation::{COMPLETION_SESSION_HEADER, DELEGATION_ID_HEADER, SPAWNER_ENV};
use capsule_runtime::{launch_session, StagedSession, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
use mur_roost::{authority::SpawnAuthority, State};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

// ── An agent capsule in queue+sleep, launched in process ──────────────────────

fn reply(index: usize, text: &str) -> String {
    serde_json::json!({
        "id": format!("msg_{index}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A project whose capsule accepts a queue of tasks and sleeps between them — the shape a parent
/// holding a delegation has, and the only shape a completion can arrive at.
fn queue_sleep_project(endpoint: &str) -> (TempDir, PathBuf) {
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
            "name: completion-lane\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\nlifecycle:\n  task_acceptance: queue\n  \
             after_task: sleep\n  queue_depth: 8\ninference:\n  transport: http\n  \
             endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    \
             artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage(home: &TempDir, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, manifest_path.parent().unwrap(), manifest_path)
}

/// Launch `staged` on a thread and hand back the address it bound, its session id and the path to
/// its trace.
struct Running {
    url: String,
    session_id: String,
    trace_path: PathBuf,
}

fn launch(staged: StagedSession) -> Running {
    let session_id = staged.session_id.clone();
    let trace_path = staged.workdir.join("trace.jsonl");
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let _ = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
    });
    let url = url_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the capsule reports its address");

    Running {
        url,
        session_id,
        trace_path,
    }
}

impl Running {
    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(&self.trace_path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn events(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn wait_for(&self, event_type: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let events = self.events(event_type);
            if events.len() >= count {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} {event_type} events; trace:\n{}",
                fs::read_to_string(&self.trace_path).unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// One `message/send`, with whatever headers the caller wants stamped on it.
    fn post(&self, message_id: &str, text: &str, headers: &[(&str, &str)]) -> Value {
        let body = serde_json::json!({
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
        .to_string();
        post_json(&self.url, &body, headers)
    }

    /// The headers a delegated child's runtime stamps on the completion it posts back.
    fn completion_headers<'a>(&'a self, delegation_id: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            (PEER_ORIGIN_HEADER, "completion"),
            (PEER_TRUST_HEADER, "trusted"),
            (DELEGATION_ID_HEADER, delegation_id),
            (COMPLETION_SESSION_HEADER, self.session_id.as_str()),
        ]
    }
}

fn post_json(addr: &str, body: &str, headers: &[(&str, &str)]) -> Value {
    let mut stream = TcpStream::connect(addr).expect("the capsule accepts a connection");
    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
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
    let mut response = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        response.push_str(&line);
    }
    serde_json::from_str(&response).unwrap_or_else(|_| serde_json::json!({"_raw": response}))
}

fn submitted(response: &Value, label: &str) -> String {
    assert_eq!(
        response["result"]["status"]["state"], "submitted",
        "task {label} should be submitted; got: {response}"
    );
    response["result"]["id"]
        .as_str()
        .expect("a submitted task carries its id")
        .to_string()
}

// ── 3. A completion lands in the background lane and preempts nothing ─────────

/// A completion posted *before* a peer task still runs after it, and only after the task already
/// running has ended. The order comes out of the real queue's own trace.
///
/// The first task arrives as `task.md`, which is the only way a task reaches the `user` lane: the
/// door accepts `peer` and `completion` from the wire and nothing else, so the highest lane an
/// inbound request can claim is `peer`. Both lanes are above the completion's, which is the
/// property under test — a completion waits behind everything anyone is waiting for.
#[test]
fn a_completion_waits_behind_everything_anyone_is_waiting_for() {
    let server = common::ScriptedServer::start_with_delay(
        (1..=3)
            .map(|i| reply(i, &format!("task {i} done")))
            .collect(),
        Duration::from_secs(2),
    );
    let (home, manifest_path) = queue_sleep_project(&server.endpoint);
    let staged = stage(&home, &manifest_path);
    fs::write(staged.workdir.join("task.md"), "the person's task").unwrap();
    let running = launch(staged);

    // The `task.md` task starts at launch and is held there by the scripted delay.
    let first = running.wait_for("task_start", 1);
    assert_eq!(first[0]["lane"], "user");
    let first_id = first[0]["task_id"].as_str().unwrap().to_string();

    let completion = submitted(
        &running.post(
            "m-completion",
            "Delegated capsule finished.\ndelegation_id: dlg_lane0001\nstatus: ok",
            &running.completion_headers("dlg_lane0001"),
        ),
        "completion",
    );
    let waiting_peer = submitted(
        &running.post(
            "m-peer",
            "a peer is blocked on this",
            &[(PEER_ORIGIN_HEADER, "peer"), (PEER_TRUST_HEADER, "trusted")],
        ),
        "peer",
    );

    let starts = running.wait_for("task_start", 3);
    let order: Vec<&str> = starts
        .iter()
        .map(|event| event["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        vec![
            first_id.as_str(),
            waiting_peer.as_str(),
            completion.as_str()
        ],
        "the peer task posted after the completion still runs first; trace:\n{}",
        fs::read_to_string(&running.trace_path).unwrap_or_default()
    );

    let lanes: Vec<&str> = starts
        .iter()
        .map(|event| event["lane"].as_str().unwrap())
        .collect();
    assert_eq!(lanes, vec!["user", "peer", "bg"]);

    let completion_start = &starts[2];
    assert_eq!(completion_start["origin"], "completion");
    assert_eq!(completion_start["delegation_id"], "dlg_lane0001");

    let first_end = running
        .events("task_end")
        .into_iter()
        .find(|event| event["task_id"] == first_id.as_str())
        .expect("the first task ended");
    assert!(
        completion_start["timestamp"].as_u64().unwrap() >= first_end["timestamp"].as_u64().unwrap(),
        "the completion started only after the running task ended: {completion_start} / {first_end}"
    );
}

// ── 4. A parent asleep when a child finishes is woken ─────────────────────────

/// Idle, with no task in flight and nothing else delivered: one completion is enough to wake the
/// loop and run a task.
#[test]
fn a_completion_wakes_a_sleeping_parent() {
    let server = common::ScriptedServer::start(vec![reply(1, "completion handled")]);
    let (home, manifest_path) = queue_sleep_project(&server.endpoint);
    let running = launch(stage(&home, &manifest_path));

    // Idle: the loop is blocked on its channel, with nothing queued and nothing running.
    thread::sleep(Duration::from_secs(2));
    assert!(
        running.events("task_start").is_empty(),
        "nothing may have run before the completion arrived"
    );

    let task = submitted(
        &running.post(
            "m-only",
            "Delegated capsule finished.\ndelegation_id: dlg_wake0001\nstatus: ok",
            &running.completion_headers("dlg_wake0001"),
        ),
        "completion",
    );

    let starts = running.wait_for("task_start", 1);
    assert_eq!(starts.len(), 1, "only the completion was delivered");
    assert_eq!(starts[0]["task_id"], task);
    assert_eq!(starts[0]["origin"], "completion");
    assert_eq!(starts[0]["lane"], "bg");
    assert_eq!(starts[0]["delegation_id"], "dlg_wake0001");

    running.wait_for("task_end", 1);
    assert!(
        running.events("session_end").is_empty(),
        "a queue+sleep session must not have ended on an idle timeout"
    );
}

// ── 8. A capsule that never delegates is unaffected ───────────────────────────

/// A real `mur-roost` on a loopback port, counting every connection it accepts.
struct CountingRoost {
    url: String,
    connections: Arc<AtomicUsize>,
    _registry: TempDir,
}

impl CountingRoost {
    fn start() -> Self {
        let registry = TempDir::new().unwrap();
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path: registry.path().to_path_buf(),
            spawn_allow: Vec::new(),
            max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
            max_concurrent: mur_roost::bounds::DEFAULT_MAX_CONCURRENT,
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));

        let accept_state = Arc::clone(&state);
        let accept_count = Arc::clone(&connections);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                accept_count.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&accept_state);
                thread::spawn(move || mur_roost::handle_connection(stream, state));
            }
        });

        Self {
            url,
            connections,
            _registry: registry,
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// A capsule with no `capabilities.spawn.allow`, no `MURMUR_SPAWNER` and no daemon runs exactly as
/// it did: it writes no completion, contacts nobody but its own inference endpoint, and its
/// `task_start` lines carry no delegation id at all.
#[test]
fn a_capsule_that_never_delegates_is_unaffected() {
    let server = common::ScriptedServer::start(vec![reply(1, "plain run done")]);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let roost = CountingRoost::start();

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
            "name: no-delegation\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    \
             allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  \
             model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n",
            endpoint = server.endpoint,
        ),
    )
    .unwrap();

    let assert = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove(SPAWNER_ENV)
        // A daemon is listening and named, and this capsule must still not reach for it.
        .env("MURMUR_ROOST_URL", &roost.url)
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
            "--task",
            "do the thing",
            "--no-env-file",
        ])
        .assert()
        .success();
    let _ = assert;

    assert_eq!(
        roost.connections(),
        0,
        "a capsule that declares no spawn capability opens no connection to the daemon"
    );
    assert!(
        find_file(project.path(), "completion.json").is_none(),
        "a capsule nobody delegated writes no completion"
    );

    let trace_path = find_file(project.path(), "trace.jsonl").expect("the session wrote a trace");
    let trace = fs::read_to_string(&trace_path).unwrap();
    let starts: Vec<Value> = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event_type"] == "task_start")
        .collect();
    assert!(!starts.is_empty(), "the capsule ran a task:\n{trace}");
    for start in &starts {
        assert!(
            start.get("delegation_id").is_none(),
            "the field is omitted from the line, not written as null: {start}"
        );
    }
}

/// The first file named `name` anywhere beneath `root`.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                return Some(path);
            }
        }
    }
    None
}

// ── 9. A malformed spawner handle refuses the launch ──────────────────────────

/// A child that cannot read the handle its parent injected cannot report its outcome to anybody,
/// so the launch is refused before a session is staged rather than run unreportable.
#[test]
fn a_malformed_spawner_handle_refuses_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: unreportable\nversion: 0.1.0\nartifacts: []\n",
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-env-echo.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    let assert = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env(SPAWNER_ENV, "not-a-spawner-handle")
        .args([
            "run",
            "--manifest",
            project.path().join("murmur.yaml").to_str().unwrap(),
            "--no-env-file",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

    assert!(stderr.contains("E-RUN-020"), "{stderr}");
    assert!(
        stderr.contains("a delegated child must be able to tell its spawner that it finished"),
        "{stderr}"
    );

    // Nothing was staged and nothing was written: no session directory, no trace.
    assert!(
        find_file(project.path(), "trace.jsonl").is_none(),
        "a refused launch writes no trace"
    );
    assert!(
        !project.path().join("workdir").exists(),
        "a refused launch stages no session"
    );
}
