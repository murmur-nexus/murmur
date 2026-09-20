//! Stopping a `transport: process` task: the harness the runtime spawned is interrupted, and
//! killed when the interrupt is refused or was never possible.
//!
//! Every case stages and launches a real capsule in-process, because a cancel needs the A2A door
//! and the property under test is what the runtime does to a process it owns: the acceptance
//! measurement is the harness pid, read with `kill(pid, 0)`, not what a command printed. The
//! harness is always the fixture fake harness — a bash script, so these tests are Unix-only — and
//! the driver is one of the three fixture components, one per `describe().interrupt` arm.

#![cfg(unix)]

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

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The grace every interrupted harness in this binary gets, short enough that a test that has to
/// wait it out still finishes quickly and long enough that a harness that honours its interrupt
/// stops well inside it.
const GRACE_MS: u64 = 4000;

/// How long an attempt that spent no grace has to end in. Shorter than [`GRACE_MS`], so an attempt
/// that only ended because the grace ran out cannot pass it.
const STOPS_BEFORE_THE_GRACE: Duration = Duration::from_secs(3);

/// How long the harness has to be gone in, measured only once the attempt has already ended: the
/// runtime reaps the process before it writes the attempt's terminal status, so this is a bound on
/// nothing but `kill(pid, 0)` agreeing.
const REAPED_WITHIN: Duration = Duration::from_secs(5);

/// How the driver a capsule is built with declares its harness is interrupted.
#[derive(Clone, Copy)]
enum Interrupt {
    StdinMessage,
    SignalInt,
    Unsupported,
}

impl Interrupt {
    fn component(self) -> PathBuf {
        common::fixture_path(match self {
            Interrupt::StdinMessage => "process-driver/tool/process-driver.wasm",
            Interrupt::SignalInt => "process-driver/tool/process-driver-signal.wasm",
            Interrupt::Unsupported => "process-driver/tool/process-driver-unsupported.wasm",
        })
    }
}

/// A scratch `$HOME` and one interrupt grace for this whole test binary.
///
/// Both are process-global: a launch staged in-process resolves the harness session map against
/// the test process's own `$HOME`, and the runtime reads the debug grace from this process's
/// environment. Set once, never per test, because every sibling test shares them. The harness
/// profile is not here — it is baked into each capsule's own wrapper script, so two cases running
/// at once cannot fight over it.
fn scratch_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        std::env::set_var("MURMUR_DEBUG_INTERRUPT_GRACE_MS", GRACE_MS.to_string());
        restore_default_sigint();
        path
    })
}

/// Put `SIGINT` back to its default disposition for this process, so the harnesses it spawns
/// inherit one they can act on.
///
/// A shell running a command as a background job sets `SIGINT` to `SIG_IGN` for it, and an ignored
/// disposition survives both `fork` and `exec` — so a suite launched with `&` would hand the fake
/// harness a `SIGINT` its `trap` cannot take (a non-interactive shell cannot trap a signal that was
/// ignored on entry). The runtime would then correctly kill it after the grace, and the signal case
/// would be measuring the launching shell rather than the runtime.
fn restore_default_sigint() {
    // SAFETY: `signal` takes a signal number and a handler by value and dereferences no pointer.
    // `SIG_DFL` is what this process would have had anyway when run in the foreground.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

// ── The capsule under test ────────────────────────────────────────────────────

struct Built {
    name: String,
    interrupt: Interrupt,
    profile: String,
    /// Whether the capsule stays up after its task. `false` ends the session with the task, which
    /// is what a case that reads `mur trace show` needs: that command renders a finished session.
    sleeps: bool,
}

/// One launched process capsule, and everything a case reads it back through.
struct Capsule {
    url: String,
    workdir: PathBuf,
    _home: TempDir,
    _project: TempDir,
    /// The file the harness wrapper reads its profile from, rewritten between two tasks of the
    /// same capsule.
    profile_file: PathBuf,
}

impl Built {
    fn new(name: &str, profile: &str) -> Self {
        Self {
            name: name.to_string(),
            interrupt: Interrupt::StdinMessage,
            profile: profile.to_string(),
            sleeps: true,
        }
    }

    fn driver(mut self, interrupt: Interrupt) -> Self {
        self.interrupt = interrupt;
        self
    }

    /// End the session with the task, so the trace it leaves is a finished one.
    fn ends_with_its_task(mut self) -> Self {
        self.sleeps = false;
        self
    }

    fn launch(self) -> Capsule {
        scratch_home();
        let home = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        let artifact = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &self.interrupt.component(),
            "",
        );
        common::publish_local(&home, &artifact).success();

        // The harness the capsule runs is a wrapper that exports the profile and execs the fixture
        // script, so the profile belongs to this capsule rather than to the test process every
        // capsule in this binary shares.
        let harness = project.path().join("fake-harness");
        fs::copy(
            common::fixture_path("process-driver/fake-harness"),
            &harness,
        )
        .unwrap();
        make_executable(&harness);
        let profile_file = project.path().join("profile");
        fs::write(&profile_file, &self.profile).unwrap();
        let wrapper = project.path().join("harness");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/bash\nexport FIXTURE_HARNESS_PROFILE=\"$(cat {})\"\nexec {} \"$@\"\n",
                profile_file.display(),
                harness.display()
            ),
        )
        .unwrap();
        make_executable(&wrapper);

        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: {}\nversion: 0.1.0\n\
                 artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
                 capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n  \
                 command: {}\n",
                self.name,
                wrapper.display()
            ),
        )
        .unwrap();

        let staged = stage(&home, &manifest, self.sleeps);
        let workdir = staged.workdir.clone();
        let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let _ = launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            });
        });
        let url = url_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("timed out waiting for the capsule URL");
        Capsule {
            url,
            workdir,
            _home: home,
            _project: project,
            profile_file,
        }
    }
}

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// A queue capsule that threads its conversation. `sleeps` keeps it up after the task, which is
/// the shape a cancel is interesting on: the session outlives the task it stopped.
fn lifecycle(sleeps: bool) -> LifecycleConfig {
    LifecycleConfig {
        task_acceptance: TaskAcceptance::Queue,
        after_task: if sleeps {
            AfterTask::Sleep
        } else {
            AfterTask::Exit
        },
        queue_depth: 8,
        conversation_mode: murmur_artifact::ConversationMode::Threaded,
        ..Default::default()
    }
}

fn stage(home: &TempDir, manifest_path: &Path, sleeps: bool) -> capsule_runtime::StagedSession {
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
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: Some(lifecycle(sleeps)),
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
    let mut body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        body.push_str(&line);
    }
    serde_json::from_str(&body).unwrap_or_else(|_| json!({"_raw": body}))
}

fn http_get(addr: &str, path: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = &stream;
    writer
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        body.push_str(&line);
    }
    serde_json::from_str(body.trim()).unwrap_or_else(|_| json!({"_raw": body}))
}

fn send_message(addr: &str, message_id: &str, text: &str, context_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {
                "messageId": message_id,
                "contextId": context_id,
                "role": "user",
                "parts": [{"text": text}]
            }}
        })
        .to_string(),
    )
}

fn tasks_get(addr: &str, task_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}})
            .to_string(),
    )
}

fn tasks_cancel(addr: &str, task_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tasks/cancel", "params": {"id": task_id}})
            .to_string(),
    )
}

/// The id of whichever task holds the active slot — `message/stream` never reports one.
fn active_task_id(addr: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let response = http_post_json(
            addr,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tasks/get", "params": {}}).to_string(),
        );
        if let Some(id) = response["result"]["id"].as_str() {
            return id.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "no task ever took the active slot; last response: {response}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn poll_until_state(addr: &str, task_id: &str, expected: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let response = tasks_get(addr, task_id);
        if response["result"]["status"]["state"].as_str() == Some(expected) {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for state '{expected}'; last response: {response}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── One streamed task ─────────────────────────────────────────────────────────

/// An open `message/stream` connection, read to its terminal frame.
struct Stream {
    reader: BufReader<TcpStream>,
}

impl Stream {
    /// Submit a task on a `message/stream` connection and read past its headers.
    fn open(addr: &str, context_id: &str) -> Self {
        let stream = TcpStream::connect(addr).expect("should connect for SSE");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/stream",
            "params": {"message": {
                "messageId": "msg-stream",
                "contextId": context_id,
                "role": "user",
                "parts": [{"text": "do the thing"}]
            }}
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
        stream.set_read_timeout(Some(Duration::from_secs(90))).ok();
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
        }
        Self { reader }
    }

    /// Every `(event, data)` pair the connection carried, read until it closed.
    fn frames(&mut self) -> Vec<(String, Value)> {
        let mut frames = Vec::new();
        let mut current = String::new();
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => break,
                Err(error) => panic!("the SSE connection errored instead of closing: {error}"),
                Ok(_) => {}
            }
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if let Some(rest) = line.strip_prefix("event: ") {
                current = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if let Ok(data) = serde_json::from_str::<Value>(rest) {
                    frames.push((current.clone(), data));
                }
            }
        }
        frames
    }
}

/// The one `"final":true` status frame a stream carried, asserting it is the only one.
fn only_terminal_status(frames: &[(String, Value)]) -> Value {
    let terminal: Vec<&Value> = frames
        .iter()
        .filter(|(event, data)| event == "status" && data["final"] == json!(true))
        .map(|(_, data)| data)
        .collect();
    assert_eq!(
        terminal.len(),
        1,
        "exactly one terminal status per attempt; got {frames:#?}"
    );
    terminal[0].clone()
}

// ── Reading the trace and the harness ─────────────────────────────────────────

impl Capsule {
    fn set_profile(&self, profile: &str) {
        fs::write(&self.profile_file, profile).unwrap();
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.workdir.join("trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
            .collect()
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// Wait for `predicate` to hold over the trace, then return it.
    fn wait_for_trace(&self, what: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let events = self.events();
            if predicate(&events) {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; trace holds: {:?}",
                events
                    .iter()
                    .map(|e| e["event_type"].as_str().unwrap_or("?").to_string())
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn one(&self, event_type: &str) -> Value {
        let mut found = self.of_type(event_type);
        assert_eq!(
            found.len(),
            1,
            "expected exactly one {event_type}, got {found:#?}"
        );
        found.remove(0)
    }

    /// The pid the running harness wrote, once it has written one.
    fn harness_pid(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(path) = common::find_file(&self.workdir, "harness.pid") {
                if let Some(pid) = fs::read_to_string(path)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the harness wrote no pid under {:?}",
                self.workdir
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn trace_show(&self) -> String {
        let output = assert_cmd::Command::cargo_bin("mur")
            .unwrap()
            .args([
                "trace",
                "show",
                self.workdir.join("trace.jsonl").to_str().unwrap(),
            ])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

/// Whether a process id is still alive, by `kill(pid, 0)`.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs the permission and existence check and delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn assert_dead_within(pid: i32, limit: Duration) {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the harness (pid {pid}) was still alive after {limit:?}");
}

/// Cancel `task_id` and return how long the door took to answer.
fn cancel_now(url: &str, task_id: &str) -> Duration {
    let started = Instant::now();
    let canceled = tasks_cancel(url, task_id);
    let took = started.elapsed();
    assert_eq!(
        canceled["result"]["status"]["state"], "canceled",
        "{canceled}"
    );
    assert!(
        took < Duration::from_secs(3),
        "the door waited on the harness: {took:?}"
    );
    took
}

// ── S1: a harness that honours its interrupt ──────────────────────────────────

/// The whole point: the harness stops, the capsule stays usable, and the context it was holding
/// is still resumable by the next task.
#[test]
fn s1_a_harness_that_honours_the_interrupt_stops_and_the_context_survives() {
    println!("S1: a stdin-message interrupt stops the harness and leaves the context resumable");
    let capsule = Built::new("process-cancel-honours", "interrupt-honours").launch();
    let context = "ctx-cancel-1";

    let submitted = send_message(&capsule.url, "msg-1", "do the thing", context);
    let task_id = submitted["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(&capsule.url, &task_id, "working", Duration::from_secs(60));
    let pid = capsule.harness_pid();

    cancel_now(&capsule.url, &task_id);
    // The harness is reaped before the attempt ends, so its records are the signal to read
    // liveness on: a pid polled before them races the runtime rather than measuring it.
    capsule.wait_for_trace("the cancelled task's records", |events| {
        events.iter().any(|e| e["event_type"] == "task_end")
    });
    assert_dead_within(pid, REAPED_WITHIN);
    assert_eq!(
        tasks_get(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
    let interrupt = capsule.one("harness_interrupt");
    assert_eq!(interrupt["method"], "stdin-message", "{interrupt}");
    assert_eq!(interrupt["delivered"], true, "{interrupt}");
    assert_eq!(interrupt["grace_ms"], GRACE_MS, "{interrupt}");
    let exit = capsule.one("harness_exit");
    assert_eq!(exit["cause"], "canceled", "{exit}");
    let canceled = capsule.one("task_canceled");
    assert_eq!(canceled["phase"], "harness", "{canceled}");
    assert_eq!(canceled["task_id"], task_id.as_str(), "{canceled}");
    let ended = capsule.one("task_end");
    assert_eq!(ended["exit_status"], "canceled", "{ended}");

    // The conversation the stopped harness opened is still this context's, so the next task
    // resumes it rather than starting from nothing.
    let harness_session = capsule.one("harness_session");
    let session_id = harness_session["harness_session_id"].as_str().unwrap();
    capsule.set_profile("happy");
    let second = send_message(&capsule.url, "msg-2", "and now this", context);
    let second_id = second["result"]["id"].as_str().unwrap().to_string();
    poll_until_state(
        &capsule.url,
        &second_id,
        "completed",
        Duration::from_secs(60),
    );

    let starts = capsule.of_type("harness_start");
    assert_eq!(starts.len(), 2, "{starts:#?}");
    assert_eq!(starts[1]["session_mode"], "resume", "{:#?}", starts[1]);
    assert_eq!(
        starts[1]["harness_session_id"], session_id,
        "the cancelled task's conversation is the one the next task continues: {:#?}",
        starts[1]
    );
}

// ── S2: a harness that ignores it ─────────────────────────────────────────────

/// A harness that never reads its stdin runs out its grace and is killed, and everything a
/// person reads afterwards says a kill was needed.
#[test]
fn s2_a_harness_that_ignores_the_interrupt_is_killed_after_the_grace() {
    println!("S2: an ignored interrupt costs the harness the grace and then its life");
    let capsule = Built::new("process-cancel-ignores", "interrupt-ignores").launch();

    let mut stream = Stream::open(&capsule.url, "ctx-cancel-2");
    let task_id = active_task_id(&capsule.url);
    let pid = capsule.harness_pid();
    let canceled_at = Instant::now();
    cancel_now(&capsule.url, &task_id);

    let status = only_terminal_status(&stream.frames());
    let took = canceled_at.elapsed();
    println!("S2: the attempt ended {took:?} after the cancel");
    assert!(
        took >= Duration::from_millis(GRACE_MS / 2),
        "the harness was killed without being given its grace: {took:?}"
    );
    assert_dead_within(pid, REAPED_WITHIN);
    assert_eq!(status["status"]["state"], "canceled", "{status}");
    let message = status["status"]["message"].as_str().unwrap();
    assert!(message.contains("the harness was killed"), "{message}");

    capsule.wait_for_trace("the cancelled task's records", |events| {
        events.iter().any(|e| e["event_type"] == "task_end")
    });
    let interrupt = capsule.one("harness_interrupt");
    assert_eq!(interrupt["method"], "stdin-message", "{interrupt}");
    assert_eq!(interrupt["delivered"], true, "{interrupt}");
    assert_eq!(interrupt["grace_ms"], GRACE_MS, "{interrupt}");
    let exit = capsule.one("harness_exit");
    assert_eq!(exit["cause"], "canceled", "{exit}");
    assert_eq!(exit["killed"], true, "{exit}");

    // And the capsule is still there to answer for it.
    assert_eq!(
        tasks_get(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
}

/// The other half of S2: that a kill was needed is readable without `jq`. Its capsule ends with
/// its task, because `mur trace show` renders a finished session.
#[test]
fn s2_mur_trace_show_prints_the_interrupt_under_harness() {
    println!("S2: `mur trace show` prints the interrupt line under ── Harness ──");
    let capsule = Built::new("process-cancel-shown", "interrupt-ignores")
        .ends_with_its_task()
        .launch();

    let _stream = Stream::open(&capsule.url, "ctx-cancel-2b");
    let task_id = active_task_id(&capsule.url);
    let pid = capsule.harness_pid();
    cancel_now(&capsule.url, &task_id);
    capsule.wait_for_trace("the session to end", |events| {
        events.iter().any(|e| e["event_type"] == "session_end")
    });
    assert_dead_within(pid, REAPED_WITHIN);

    let shown = capsule.trace_show();
    assert!(shown.contains("── Harness ──"), "{shown}");
    assert!(
        shown.contains("interrupt stdin-message: sent, 4000ms grace"),
        "{shown}"
    );
}

// ── S3: SIGINT ────────────────────────────────────────────────────────────────

/// A driver that declares `signal-int` has its harness sent `SIGINT`, and a harness that honours
/// one stops well inside the grace.
#[test]
fn s3_a_harness_interrupted_by_sigint_stops() {
    println!("S3: a signal-int driver's harness is stopped with SIGINT");
    let capsule = Built::new("process-cancel-signal", "interrupt-signal")
        .driver(Interrupt::SignalInt)
        .launch();

    let mut stream = Stream::open(&capsule.url, "ctx-cancel-3");
    let task_id = active_task_id(&capsule.url);
    let pid = capsule.harness_pid();
    let canceled_at = Instant::now();
    cancel_now(&capsule.url, &task_id);

    let status = only_terminal_status(&stream.frames());
    let took = canceled_at.elapsed();
    println!("S3: the attempt ended {took:?} after the cancel");
    assert!(
        took < STOPS_BEFORE_THE_GRACE,
        "the signal was waited out rather than honoured: {took:?}"
    );
    assert_dead_within(pid, REAPED_WITHIN);
    assert_eq!(status["status"]["state"], "canceled", "{status}");
    assert_eq!(
        status["status"]["message"], "task canceled",
        "a harness that stopped when it was asked leaves the http path's message: {status}"
    );

    capsule.wait_for_trace("the cancelled task's records", |events| {
        events.iter().any(|e| e["event_type"] == "task_end")
    });
    let interrupt = capsule.one("harness_interrupt");
    assert_eq!(interrupt["method"], "signal-int", "{interrupt}");
    assert_eq!(interrupt["delivered"], true, "{interrupt}");
    assert_eq!(capsule.one("harness_exit")["cause"], "canceled");
}

// ── S4: no graceful interrupt at all ──────────────────────────────────────────

/// A driver that declares `unsupported` spends no grace: a person can always stop the task, so
/// the harness is killed at once.
#[test]
fn s4_a_driver_declaring_unsupported_has_its_harness_killed_at_once() {
    println!("S4: an unsupported interrupt-method kills the harness immediately");
    let capsule = Built::new("process-cancel-unsupported", "interrupt-ignores")
        .driver(Interrupt::Unsupported)
        .launch();

    let mut stream = Stream::open(&capsule.url, "ctx-cancel-4");
    let task_id = active_task_id(&capsule.url);
    let pid = capsule.harness_pid();
    let canceled_at = Instant::now();
    cancel_now(&capsule.url, &task_id);

    let status = only_terminal_status(&stream.frames());
    let took = canceled_at.elapsed();
    println!("S4: the attempt ended {took:?} after the cancel");
    assert!(
        took < STOPS_BEFORE_THE_GRACE,
        "a driver with no graceful interrupt spends no grace: {took:?}"
    );
    assert_dead_within(pid, REAPED_WITHIN);
    assert_eq!(status["status"]["state"], "canceled", "{status}");
    let message = status["status"]["message"].as_str().unwrap();
    assert!(
        message.contains("the harness was killed and its session may not resume cleanly"),
        "{message}"
    );

    capsule.wait_for_trace("the cancelled task's records", |events| {
        events.iter().any(|e| e["event_type"] == "task_end")
    });
    let interrupt = capsule.one("harness_interrupt");
    assert_eq!(interrupt["method"], "unsupported", "{interrupt}");
    assert_eq!(interrupt["delivered"], false, "{interrupt}");
    assert_eq!(interrupt["grace_ms"], 0, "{interrupt}");
    let exit = capsule.one("harness_exit");
    assert_eq!(exit["cause"], "canceled", "{exit}");
    assert_eq!(exit["killed"], true, "{exit}");
}

// ── S5: one terminal status, and it is canceled ───────────────────────────────

/// The invariant the A2A half keeps: exactly one `final:true` status per attempt, and a cancelled
/// attempt's is `canceled` — never a `completed` or `failed` alongside it.
#[test]
fn s5_a_cancelled_task_writes_exactly_one_terminal_status() {
    println!("S5: a cancelled process task closes its stream with one canceled status");
    let capsule = Built::new("process-cancel-stream", "interrupt-honours").launch();
    let context = "ctx-cancel-5";

    let mut stream = Stream::open(&capsule.url, context);
    let task_id = active_task_id(&capsule.url);
    capsule.harness_pid();
    cancel_now(&capsule.url, &task_id);

    let frames = stream.frames();
    let status = only_terminal_status(&frames);
    assert_eq!(status["status"]["state"], "canceled", "{status}");
    assert_eq!(status["context_id"], context, "{status}");
    for (event, data) in &frames {
        if event == "status" {
            let state = data["status"]["state"].as_str().unwrap_or_default();
            assert!(
                state != "completed" && state != "failed",
                "a stopped task is neither completed nor failed: {data}"
            );
        }
    }
}

// ── S6: the agent card ────────────────────────────────────────────────────────

/// Every process driver capsule advertises cancellation, because every one can now be stopped.
#[test]
fn s6_a_process_capsule_advertises_cancellation() {
    println!("S6: a process capsule's card advertises cancellation");
    let capsule = Built::new("process-cancel-card", "happy").launch();
    let card = http_get(&capsule.url, "/.well-known/agent-card.json");

    assert_eq!(card["capabilities"]["cancellation"], true, "{card}");
    assert_eq!(
        card["capabilities"]["streaming"], true,
        "the fixture driver reports streams-text: {card}"
    );
    assert!(
        card["serves"]["methods"]
            .as_array()
            .unwrap()
            .contains(&Value::from("tasks/cancel")),
        "{card}"
    );
}

// ── S9: the driver is told the run was interrupted ────────────────────────────

/// The runner hands `classify-exit` an `interrupted: true` exit, which is the only thing that can
/// produce the fixture driver's `canceled` classification — and it still does not make the task
/// a failed one.
#[test]
fn s9_an_interrupted_run_hands_the_driver_interrupted_true() {
    println!("S9: an interrupted run's exit is classified with interrupted: true");
    let capsule = Built::new("process-cancel-classify", "interrupt-ignores").launch();

    let mut stream = Stream::open(&capsule.url, "ctx-cancel-9");
    let task_id = active_task_id(&capsule.url);
    let pid = capsule.harness_pid();
    cancel_now(&capsule.url, &task_id);

    let status = only_terminal_status(&stream.frames());
    assert_dead_within(pid, REAPED_WITHIN);
    assert_eq!(status["status"]["state"], "canceled", "{status}");
    let message = status["status"]["message"].as_str().unwrap();
    assert!(
        !message.contains("E-RUN-033"),
        "a stopped task reports no turn failure: {message}"
    );

    capsule.wait_for_trace("the classified exit", |events| {
        events.iter().any(|e| e["event_type"] == "harness_failed")
    });
    let failed = capsule.one("harness_failed");
    assert_eq!(failed["kind"], "canceled", "{failed}");
    assert_eq!(
        failed["message"], "interrupted",
        "only the driver's exit.interrupted arm produces this: {failed}"
    );
    assert_eq!(
        tasks_get(&capsule.url, &task_id)["result"]["status"]["state"],
        "canceled"
    );
}
