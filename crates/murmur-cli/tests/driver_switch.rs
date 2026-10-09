//! Switching a running capsule's driver: a capsule whose `murmur.yaml` declares
//! `inference.alternates` can be moved between its declared driver choices while it runs, by a
//! controller holding its control token or, when granted, by the agent itself.
//!
//! Every capsule here is its own `mur run` process under a scratch `HOME`. Upstream A is a
//! recording server behind the anthropic fixture driver and upstream B one behind the openai
//! fixture driver, so every assertion is about bytes that crossed a socket or a file a process
//! wrote.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use common::idle_capsule::{http_post_json, wait_for_task_ends};
use common::leaks::{files_containing, leak_forms};
use common::recording_upstream::{RecordedRequest, RecordingUpstream, Reply};
use serde_json::{json, Value};
use tempfile::TempDir;

const ANTHROPIC: &str = "murmur-driver-anthropic";
const OPENAI: &str = "murmur-driver-openai";
const VERSION: &str = "0.1.0";
const CAPSULE: &str = "switch-agent";
const SONNET: &str = "claude-sonnet-4-5";
const HAIKU: &str = "claude-haiku-4-5";
const GPT: &str = "gpt-5";
/// Upstream A's key, written literally: these tests are about which key goes where.
const A_KEY: &str = "sw-anthropic-key-7c21e0";
/// Upstream B's key when it is written literally.
const B_KEY: &str = "sw-openai-key-3f9a44";
/// The `control.secrets` name that keys B in the injection cases.
const ALT_SECRET: &str = "SWITCH_ALT_KEY";
/// A `${NAME}` no config and no environment holds.
const MISSING: &str = "SWITCH_MISSING_KEY_4D1";
const CONTEXT: &str = "ctx-switch";
const BEARER_AUTH: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
const THREADED: &str =
    "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  conversation: threaded\n";
const GPT_ALTERNATE: &str =
    "    - name: gpt\n      model: gpt-5\n      driver:\n        artifact: murmur-driver-openai\n";
const HAIKU_ALTERNATE: &str = "    - name: haiku\n      model: claude-haiku-4-5\n      driver:\n        artifact: murmur-driver-anthropic\n";
const CONTROLLER: &str = "control:\n  settings: [inference.driver]\n";
const AGENT: &str = "control:\n  agent_settings: [inference.driver]\n";

// ── homes, projects and commands ────────────────────────────────────────────

/// A scratch `$HOME` with both fixture drivers published into it.
struct Home {
    dir: TempDir,
    _artifacts: TempDir,
}

impl Home {
    fn new() -> Arc<Self> {
        let dir = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let anthropic = common::create_driver_artifact(
            artifacts.path(),
            ANTHROPIC,
            VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&dir, &anthropic).success();
        let openai = common::create_driver_artifact_with_auth(
            artifacts.path(),
            OPENAI,
            VERSION,
            &common::fixture_path("drivers/openai/driver/murmur-driver-openai.wasm"),
            BEARER_AUTH,
        );
        common::publish_local(&dir, &openai).success();
        Arc::new(Self {
            dir,
            _artifacts: artifacts,
        })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn conversation(&self) -> Vec<Value> {
        let path = self
            .path()
            .join(".murmur/conversations")
            .join(CAPSULE)
            .join(CONTEXT)
            .join("conversation.jsonl");
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("no record at {}: {err}", path.display()))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn mur(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .env_remove(ALT_SECRET)
        .env_remove(MISSING);
    command
}

/// What one `mur` invocation exited with and printed.
#[derive(Clone, Debug)]
struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn of(output: std::process::Output) -> Self {
        Self {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }

    fn holds(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }
}

/// `mur control <args>` against `home`, with `stdin` piped in when given.
fn mur_control(home: &Path, args: &[&str], stdin: Option<&[u8]>) -> Ran {
    let mut child = mur(home)
        .arg("control")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    Ran::of(child.wait_with_output().unwrap())
}

fn set_driver(home: &Path, choice: &str) -> Ran {
    mur_control(home, &["set", "inference.driver", choice], None)
}

/// A project directory holding one `murmur.yaml`.
struct Project {
    dir: TempDir,
}

impl Project {
    fn manifest_path(&self) -> PathBuf {
        self.dir.path().join("murmur.yaml")
    }
}

/// What a capsule declares beyond its anthropic primary on sonnet.
#[derive(Default)]
struct Spec<'a> {
    /// Upstream B's endpoint, which declares the openai driver entry.
    openai: Option<&'a str>,
    /// B's `gateway.api_key`, [`B_KEY`] when empty.
    openai_key: &'a str,
    /// The body of `inference.alternates`, empty for no key.
    alternates: &'a str,
    /// Lines under `inference:` after `driver.artifact`.
    inference_extra: &'a str,
    /// Top-level blocks: `control:`, `lifecycle:`.
    top: &'a str,
    /// Extra `artifacts:` entries.
    artifacts: &'a str,
}

fn project(anthropic: &str, spec: Spec<'_>) -> Project {
    let dir = TempDir::new().unwrap();
    let openai = spec
        .openai
        .map(|endpoint| {
            format!(
                "  - name: {OPENAI}\n    version: {VERSION}\n    runtime: driver\n    gateway:\n      \
                 endpoint: {endpoint}\n      api_key: {}\n",
                if spec.openai_key.is_empty() {
                    B_KEY
                } else {
                    spec.openai_key
                }
            )
        })
        .unwrap_or_default();
    let alternates = if spec.alternates.is_empty() {
        String::new()
    } else {
        format!("  alternates:\n{}", spec.alternates)
    };
    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE}\nversion: 0.1.0\nartifacts:\n  - name: {ANTHROPIC}\n    version: \
             {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {anthropic}\n      \
             api_key: {A_KEY}\n{openai}{}inference:\n  transport: http\n  model: {SONNET}\n  \
             max_tokens: 4096\n  driver:\n    artifact: {ANTHROPIC}\n{}{alternates}{}",
            spec.artifacts, spec.inference_extra, spec.top
        ),
    )
    .unwrap();
    Project { dir }
}

/// `mur run --task` on `project`, to the end of its one task.
fn run_task(home: &Home, project: &Project) -> Ran {
    Ran::of(
        mur(home.path())
            .args(["run", "--manifest"])
            .arg(project.manifest_path())
            .args(["--task", "do the task", "--verbose", "--context", CONTEXT])
            .current_dir(project.dir.path())
            .output()
            .unwrap(),
    )
}

fn trace_events(workdir: &Path) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_else(|err| panic!("no trace in {}: {err}", workdir.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn of_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// The agent loop's own `inference` lines: every one without a hook's `origin`.
fn turns(events: &[Value]) -> Vec<&Value> {
    of_type(events, "inference")
        .into_iter()
        .filter(|event| event.get("origin").is_none())
        .collect()
}

fn trace_show(home: &Home, workdir: &Path) -> Ran {
    Ran::of(
        mur(home.path())
            .args(["trace", "show"])
            .arg(workdir.join("trace.jsonl"))
            .output()
            .unwrap(),
    )
}

fn body(request: &RecordedRequest) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn tool_names(request: &RecordedRequest) -> Vec<String> {
    body(request)["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect()
}

// ── provider replies ────────────────────────────────────────────────────────

fn anthropic_text(text: &str) -> Reply {
    Reply::json(
        json!({
            "id": "msg_reply",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    )
}

fn anthropic_tool(id: &str, name: &str, input: Value) -> Reply {
    Reply::json(
        json!({
            "id": "msg_tool",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    )
}

/// A streamed Anthropic reply carrying one signed `thinking` block and then `text`: the one shape
/// in which the driver keeps the block, with its signature, in the turn it returns.
fn anthropic_thinking(signature: &str, text: &str) -> Reply {
    let events = [
        (
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_think", "type": "message", "role": "assistant", "content": [], "model": "test-model", "stop_reason": Value::Null, "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": format!("reasoning behind {text}")}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": signature}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": text}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ];
    let body: String = events
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect();
    Reply {
        status: 200,
        content_type: "text/event-stream",
        body,
    }
}

/// An OpenAI Responses reply saying `text`: `gpt-5` is served on the Responses surface.
fn openai_text(text: &str) -> Reply {
    Reply::json(
        json!({
            "id": "resp_b",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    )
}

// ── idle capsules ───────────────────────────────────────────────────────────

/// A queue+sleep capsule running as its own `mur run --json` process, its stderr kept.
struct Idle {
    child: Child,
    startup: Value,
    stderr: Arc<Mutex<String>>,
}

impl Idle {
    fn start(home: &Home, project: &Project) -> Self {
        let mut child = mur(home.path())
            .args(["run", "--manifest"])
            .arg(project.manifest_path())
            .arg("--json")
            .current_dir(project.dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.get("url").is_some() {
                        let _ = tx.send(value);
                    }
                }
            }
        });
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr_text);
        let stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        // Each launch stages under a scratch HOME whose `~/.murmur/compiled` is empty, so it
        // compiles both fixture drivers with a debug-built Cranelift, and libtest starts one such
        // launch per test thread at once. On a loaded host the door opens after more than two
        // minutes.
        match rx.recv_timeout(Duration::from_secs(240)) {
            Ok(startup) => Self {
                child,
                startup,
                stderr: stderr_text,
            },
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "the capsule never printed where its door is; stderr:\n{}",
                    stderr_text.lock().unwrap()
                );
            }
        }
    }

    fn addr(&self) -> String {
        let port = self.startup["url"]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .to_string();
        format!("127.0.0.1:{port}")
    }

    fn workdir(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap())
    }

    fn events(&self) -> Vec<Value> {
        trace_events(&self.workdir())
    }

    /// Everything the capsule has printed to stderr by now, waiting up to five seconds for
    /// `needle` to appear.
    fn stderr_once_it_holds(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = self.stderr.lock().unwrap().clone();
            if text.contains(needle) || Instant::now() > deadline {
                return text;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Sends the capsule `SIGTERM`, which it handles with its full teardown, and waits for the
    /// process to exit, so its trace is closed with `session_end`.
    fn stop(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("the capsule did not exit after SIGTERM");
    }

    /// Sends task `n` on [`CONTEXT`] and waits for its `task_end`.
    fn task(&self, n: usize, text: &str) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": n,
            "method": "SendMessage",
            "params": {"message": {
                "messageId": format!("switch-{n}"),
                "role": "ROLE_USER",
                "contextId": CONTEXT,
                "parts": [{"text": text, "mediaType": "text/plain"}]
            }}
        })
        .to_string();
        let response = http_post_json(&self.addr(), &request);
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_SUBMITTED",
            "{response}"
        );
        wait_for_task_ends(&self.workdir(), n);
    }
}

impl Drop for Idle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── S1: a controller switch across threaded tasks ───────────────────────────

/// A controller moves the capsule `primary` → `gpt` → `primary` between three threaded tasks.
/// Each task goes to the provider selected at the time, with that provider's model and key, and
/// the conversation crosses both switches in the neutral format.
#[test]
fn s1_a_controller_switches_primary_gpt_primary_across_threaded_tasks() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|index, _| {
        anthropic_text(if index == 0 {
            "answer-one-alpha"
        } else {
            "answer-three-gamma"
        })
    });
    let b = RecordingUpstream::with_handler(|_, _| openai_text("answer-two-beta"));
    let top = format!("{CONTROLLER}{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            top: &top,
            ..Spec::default()
        },
    );
    let mut capsule = Idle::start(&home, &project);

    capsule.task(1, "question-one-alpha");
    let switched = set_driver(home.path(), "gpt");
    assert!(switched.ok, "{}", switched.context());
    assert_eq!(
        switched.stdout,
        "setting:  inference.driver\nprevious: primary\nvalue:    gpt\napplies:  next inference call\n"
    );
    capsule.task(2, "question-two-beta");
    let back = set_driver(home.path(), "primary");
    assert!(back.ok, "{}", back.context());
    assert!(back.stdout.contains("previous: gpt"), "{}", back.context());
    capsule.task(3, "question-three-gamma");

    let a_requests = a.requests();
    assert_eq!(a_requests.len(), 2, "A serves tasks 1 and 3");
    for request in &a_requests {
        assert_eq!(body(request)["model"], SONNET);
        assert_eq!(request.header_values("x-api-key"), vec![A_KEY]);
        assert!(request.header_values("authorization").is_empty());
    }
    assert!(a_requests[1].body_text().contains("answer-two-beta"));

    let b_requests = b.requests();
    assert_eq!(b_requests.len(), 1, "B serves task 2 alone");
    let b_request = &b_requests[0];
    let b_body = body(b_request);
    assert_eq!(b_body["model"], GPT);
    assert!(
        b_body.get("input").is_some(),
        "OpenAI Responses shape: {b_body}"
    );
    assert_eq!(
        b_request.header_values("authorization"),
        vec![format!("Bearer {B_KEY}").as_str()]
    );
    assert!(b_request.header_values("x-api-key").is_empty());
    let b_text = b_request.body_text();
    for carried in [
        "question-one-alpha",
        "answer-one-alpha",
        "question-two-beta",
    ] {
        assert!(b_text.contains(carried), "{carried} not in {b_text}");
    }
    for absent in ["continuation_id", "produced_by", "msg_", A_KEY] {
        assert!(!b_text.contains(absent), "{absent} in {b_text}");
    }

    let events = capsule.events();
    let changes = of_type(&events, "control_change");
    assert_eq!(changes.len(), 2, "{changes:?}");
    for (change, (previous, value)) in changes.iter().zip([("primary", "gpt"), ("gpt", "primary")])
    {
        assert_eq!(change["principal"], "controller");
        assert!(change["token_id"].as_str().is_some());
        assert_eq!(change["name"], "inference.driver");
        assert_eq!(change["previous"], previous);
        assert_eq!(change["value"], value);
        assert!(change["timestamp"].as_u64().is_some());
    }
    let applied = of_type(&events, "control_applied");
    assert_eq!(applied.len(), 2, "{applied:?}");
    for (applied, change) in applied.iter().zip(&changes) {
        assert_eq!(applied["name"], "inference.driver");
        assert_eq!(applied["value"], change["value"]);
        assert_eq!(applied["change_id"], change["event_id"]);
        assert_eq!(applied["turn"], 0);
    }
    let served: Vec<(String, String)> = turns(&events)
        .iter()
        .map(|turn| {
            (
                turn["driver_choice"].as_str().unwrap().to_string(),
                turn["model"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        served,
        vec![
            ("primary".to_string(), SONNET.to_string()),
            ("gpt".to_string(), GPT.to_string()),
            ("primary".to_string(), SONNET.to_string()),
        ]
    );

    capsule.stop();
    let show = trace_show(&home, &capsule.workdir());
    assert!(show.ok, "{}", show.context());
    assert!(
        show.stdout.contains(
            "setting inference.driver  primary \u{2192} gpt  applied from turn 0  by controller"
        ),
        "{}",
        show.context()
    );
    assert!(show.stdout.contains("on gpt (gpt-5)"), "{}", show.context());
    assert!(
        show.stdout.contains("on primary (claude-sonnet-4-5)"),
        "{}",
        show.context()
    );
}

// ── S2: an undeclared choice ────────────────────────────────────────────────

/// A name that is not a declared choice — a model, an artifact name, a number — is refused with
/// `422`, the CLI says which choices exist, and nothing changes.
#[test]
fn s2_an_undeclared_choice_is_refused_and_changes_nothing() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|_, _| anthropic_text("still-on-a"));
    let b = RecordingUpstream::with_handler(|_, _| openai_text("never"));
    let top = format!("{CONTROLLER}{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            top: &top,
            ..Spec::default()
        },
    );
    let capsule = Idle::start(&home, &project);

    for refused in ["claude-opus", OPENAI, "3"] {
        let ran = set_driver(home.path(), refused);
        assert!(!ran.ok, "{refused}: {}", ran.context());
        assert!(ran.stderr.contains("E-RUN-042"), "{}", ran.context());
        assert!(ran.stderr.contains("HTTP 422"), "{}", ran.context());
        assert!(ran.stderr.contains("primary, gpt"), "{}", ran.context());
    }
    let show = mur_control(home.path(), &["show"], None);
    assert!(show.ok, "{}", show.context());
    assert!(
        show.stdout.contains("inference.driver   primary"),
        "{}",
        show.context()
    );
    assert!(
        show.stdout
            .contains("model gpt-5  driver murmur-driver-openai  available"),
        "{}",
        show.context()
    );

    capsule.task(1, "question");
    assert_eq!(a.requests().len(), 1);
    assert!(b.requests().is_empty(), "B received a request");

    let events = capsule.events();
    assert!(of_type(&events, "control_change").is_empty());
    let refusals = of_type(&events, "control_refused");
    assert_eq!(refusals.len(), 3, "{refusals:?}");
    for refusal in refusals {
        assert_eq!(refusal["status"], 422);
        assert_eq!(refusal["reason"], "undeclared_driver");
        assert_eq!(refusal["principal"], "controller");
        assert_eq!(refusal["name"], "inference.driver");
    }
    assert_eq!(turns(&events)[0]["driver_choice"], "primary");
}

// ── S3: a credential that cannot be resolved ────────────────────────────────

/// An alternate keyed by a `control.secrets` name: refused until the secret is injected, then
/// served with the injected key, and the secret cannot be forgotten while its choice is in use.
#[test]
fn s3_an_injected_alternate_credential_gates_the_switch_and_the_forget() {
    let home = Home::new();
    let marker = format!("sw-injected-{}", std::process::id());
    let a = RecordingUpstream::with_handler(|_, _| anthropic_text("from-a"));
    let b = RecordingUpstream::with_handler(|_, _| openai_text("from-b"));
    let key = format!("${{{ALT_SECRET}}}");
    let top =
        format!("control:\n  settings: [inference.driver]\n  secrets: [{ALT_SECRET}]\n{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            openai_key: &key,
            alternates: GPT_ALTERNATE,
            top: &top,
            ..Spec::default()
        },
    );
    let capsule = Idle::start(&home, &project);
    let mut ran = Vec::new();

    let early = set_driver(home.path(), "gpt");
    assert!(!early.ok, "{}", early.context());
    assert!(early.stderr.contains("HTTP 409"), "{}", early.context());
    assert!(early.stderr.contains(ALT_SECRET), "{}", early.context());
    ran.push(early);
    capsule.task(1, "question-one");
    assert_eq!(a.requests().len(), 1);
    assert!(b.requests().is_empty());

    let secret = mur_control(
        home.path(),
        &["secret", ALT_SECRET],
        Some(format!("{marker}\n").as_bytes()),
    );
    assert!(secret.ok, "{}", secret.context());
    ran.push(secret);
    let switched = set_driver(home.path(), "gpt");
    assert!(switched.ok, "{}", switched.context());
    ran.push(switched);
    let held = mur_control(home.path(), &["forget", ALT_SECRET], None);
    assert!(!held.ok, "{}", held.context());
    assert!(held.stderr.contains("HTTP 409"), "{}", held.context());
    ran.push(held);

    capsule.task(2, "question-two");
    let b_requests = b.requests();
    assert_eq!(b_requests.len(), 1);
    assert_eq!(
        b_requests[0].header_values("authorization"),
        vec![format!("Bearer {marker}").as_str()]
    );

    let back = set_driver(home.path(), "primary");
    assert!(back.ok, "{}", back.context());
    ran.push(back);
    let forgot = mur_control(home.path(), &["forget", ALT_SECRET], None);
    assert!(forgot.ok, "{}", forgot.context());
    ran.push(forgot);

    let events = capsule.events();
    let reasons: Vec<&str> = of_type(&events, "control_refused")
        .iter()
        .map(|event| event["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, vec!["credential_unresolvable", "in_use"]);
    let choices = &of_type(&events, "session_start")[0]["inference_choices"];
    assert_eq!(choices[1]["name"], "gpt");
    assert_eq!(choices[1]["credential_source"], "injected");
    assert_eq!(choices[1]["available"], true);

    let workdir = capsule.workdir();
    let bodies: Vec<Vec<u8>> = a
        .requests()
        .iter()
        .chain(b_requests.iter())
        .map(|request| request.body.clone())
        .collect();
    drop(capsule);
    for form in leak_forms(&marker) {
        for root in [home.path(), project.dir.path(), workdir.as_path()] {
            let leaks = files_containing(root, form.as_bytes());
            assert!(
                leaks.is_empty(),
                "{form} under {}: {leaks:?}",
                root.display()
            );
        }
        for (index, output) in ran.iter().enumerate() {
            assert!(!output.holds(&form), "{form} in output {index}");
        }
        for body in &bodies {
            assert!(!body.windows(form.len()).any(|w| w == form.as_bytes()));
        }
    }
}

/// An alternate whose `${NAME}` is found nowhere at launch: the launch succeeds on the primary,
/// `W-RUN-003` names the choice, the trace marks it unavailable, and a switch to it is refused.
#[test]
fn s3_an_alternate_whose_credential_is_missing_at_launch_is_staged_unavailable() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|_, _| anthropic_text("from-a"));
    let b = RecordingUpstream::with_handler(|_, _| openai_text("never"));
    let key = format!("${{{MISSING}}}");
    let top = format!("{CONTROLLER}{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            openai_key: &key,
            alternates: GPT_ALTERNATE,
            top: &top,
            ..Spec::default()
        },
    );
    let capsule = Idle::start(&home, &project);

    let stderr = capsule.stderr_once_it_holds("W-RUN-003");
    let warning = stderr
        .lines()
        .find(|line| line.contains("warning[W-RUN-003]"))
        .unwrap_or_else(|| panic!("no W-RUN-003 in:\n{stderr}"));
    for named in ["'gpt'", OPENAI, MISSING] {
        assert!(warning.contains(named), "{named} not in {warning}");
    }

    let events = capsule.events();
    assert_eq!(
        of_type(&events, "session_start")[0]["inference_choices"],
        json!([
            {"name": "primary", "driver": ANTHROPIC, "model": SONNET, "credential_source": "manifest", "available": true},
            {"name": "gpt", "driver": OPENAI, "model": GPT, "credential_source": "none", "available": false},
        ])
    );
    let show = mur_control(home.path(), &["show"], None);
    assert!(
        show.stdout
            .contains("model gpt-5  driver murmur-driver-openai  unavailable"),
        "{}",
        show.context()
    );
    let refused = set_driver(home.path(), "gpt");
    assert!(!refused.ok, "{}", refused.context());
    assert!(
        refused.stderr.contains("E-RUN-042"),
        "{}",
        refused.context()
    );
    assert!(refused.stderr.contains("HTTP 409"), "{}", refused.context());
    assert!(refused.stderr.contains(MISSING), "{}", refused.context());

    capsule.task(1, "question");
    assert_eq!(a.requests().len(), 1);
    assert!(b.requests().is_empty());
}

// ── S4: no agent grant ──────────────────────────────────────────────────────

/// Without `control.agent_settings`, `switch-driver` does not exist: it is in neither the tool
/// array nor `tools_declared` nor the workdir, and a call to it anyway is refused naming the
/// declaration it needs.
#[test]
fn s4_without_the_agent_grant_switch_driver_does_not_exist() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|index, _| {
        if index == 0 {
            anthropic_tool("toolu_s4", "switch-driver", json!({"driver": "gpt"}))
        } else {
            anthropic_text("done")
        }
    });
    let b = RecordingUpstream::with_handler(|_, _| openai_text("never"));
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            top: CONTROLLER,
            ..Spec::default()
        },
    );
    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let requests = a.requests();
    assert_eq!(requests.len(), 2);
    assert!(!tool_names(&requests[0]).contains(&"switch-driver".to_string()));
    assert!(requests[1]
        .body_text()
        .contains("control.agent_settings: [inference.driver]"));
    assert!(b.requests().is_empty(), "B received a request");

    let workdir = common::parse_workdir_from_stdout(&run.stdout);
    assert!(!workdir.join("tools").join("switch-driver").exists());
    let events = trace_events(&workdir);
    let declared = &of_type(&events, "session_start")[0]["tools_declared"];
    assert!(!declared
        .as_array()
        .unwrap()
        .contains(&json!("switch-driver")));
    assert!(of_type(&events, "control_change").is_empty());
    assert!(turns(&events)
        .iter()
        .all(|turn| turn["driver_choice"] == "primary"));
}

/// `switch-driver` is the runtime's name whether or not the capsule declares the grant.
#[test]
fn s4_an_artifact_named_switch_driver_is_refused() {
    let home = Home::new();
    let project = project(
        "http://127.0.0.1:9",
        Spec {
            artifacts: "  - name: switch-driver\n    version: 0.1.0\n    runtime: tool\n",
            ..Spec::default()
        },
    );
    let run = run_task(&home, &project);
    assert!(!run.ok, "{}", run.context());
    assert!(run.stderr.contains("E-CAP-013"), "{}", run.context());
    assert!(run.stderr.contains("switch-driver"), "{}", run.context());
}

// ── S5: the agent switches itself ───────────────────────────────────────────

/// With the grant the model calls `switch-driver`, and the same task's next call goes to B with
/// the call and its result in it. The trace says the agent made the change.
#[test]
fn s5_the_agent_switches_itself_with_the_grant() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|_, _| {
        anthropic_tool("toolu_s5", "switch-driver", json!({"driver": "gpt"}))
    });
    let b = RecordingUpstream::with_handler(|_, _| openai_text("answered-by-gpt"));
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            top: AGENT,
            ..Spec::default()
        },
    );
    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let a_requests = a.requests();
    assert_eq!(a_requests.len(), 1);
    assert!(tool_names(&a_requests[0]).contains(&"switch-driver".to_string()));
    let b_requests = b.requests();
    assert_eq!(b_requests.len(), 1);
    assert_eq!(body(&b_requests[0])["model"], GPT);
    let b_text = b_requests[0].body_text();
    assert!(b_text.contains("toolu_s5"), "{b_text}");
    assert!(b_text.contains("switched from primary to gpt"), "{b_text}");
    assert!(!b_text.contains("produced_by"), "{b_text}");

    let workdir = common::parse_workdir_from_stdout(&run.stdout);
    let events = trace_events(&workdir);
    let changes = of_type(&events, "control_change");
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0]["principal"], "agent");
    assert!(changes[0].get("token_id").is_none(), "{}", changes[0]);
    assert_eq!(changes[0]["previous"], "primary");
    assert_eq!(changes[0]["value"], "gpt");
    let applied = of_type(&events, "control_applied");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0]["turn"], 1);
    assert_eq!(applied[0]["change_id"], changes[0]["event_id"]);
    let served: Vec<&str> = turns(&events)
        .iter()
        .map(|turn| turn["driver_choice"].as_str().unwrap())
        .collect();
    assert_eq!(served, vec!["primary", "gpt"]);

    let show = trace_show(&home, &workdir);
    assert!(
        show.stdout.contains(
            "setting inference.driver  primary \u{2192} gpt  applied from turn 1  by agent"
        ),
        "{}",
        show.context()
    );
}

/// The agent asking for a name that is not declared gets a tool error, the trace records the
/// refusal as the agent's, and nothing switches.
#[test]
fn s5_an_undeclared_name_from_the_agent_is_a_tool_error() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|index, _| {
        if index == 0 {
            anthropic_tool("toolu_nope", "switch-driver", json!({"driver": "nope"}))
        } else {
            anthropic_text("stayed")
        }
    });
    let b = RecordingUpstream::with_handler(|_, _| openai_text("never"));
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            top: AGENT,
            ..Spec::default()
        },
    );
    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let requests = a.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].body_text();
    assert!(second.contains("primary, gpt"), "{second}");
    assert!(second.contains("toolu_nope"), "{second}");
    assert!(b.requests().is_empty());

    let events = trace_events(&common::parse_workdir_from_stdout(&run.stdout));
    assert!(of_type(&events, "control_change").is_empty());
    let refusals = of_type(&events, "control_refused");
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0]["principal"], "agent");
    assert_eq!(refusals[0]["reason"], "undeclared_driver");
    assert_eq!(refusals[0]["name"], "inference.driver");
    assert!(refusals[0].get("token_id").is_none());
}

// ── S6: signed reasoning ────────────────────────────────────────────────────

/// A `thinking` block sonnet signed reaches sonnet and no other model: not haiku after a switch,
/// and sonnet again after the switch back. The record carries each turn's producer; no request
/// does.
#[test]
fn s6_signed_thinking_goes_only_to_the_model_that_wrote_it() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|index, _| match index {
        0 => anthropic_thinking("sig-sonnet-1", "answer-one"),
        1 => anthropic_thinking("sig-haiku-1", "answer-two"),
        _ => anthropic_text("answer-three"),
    });
    let top = format!("{CONTROLLER}{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            alternates: HAIKU_ALTERNATE,
            inference_extra: "    config:\n      thinking: enabled\n",
            top: &top,
            ..Spec::default()
        },
    );
    let capsule = Idle::start(&home, &project);

    capsule.task(1, "question-one");
    assert!(set_driver(home.path(), "haiku").ok);
    capsule.task(2, "question-two");
    assert!(set_driver(home.path(), "primary").ok);
    capsule.task(3, "question-three");

    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    let models: Vec<Value> = requests
        .iter()
        .map(|request| body(request)["model"].clone())
        .collect();
    assert_eq!(models, vec![json!(SONNET), json!(HAIKU), json!(SONNET)]);
    let to_haiku = requests[1].body_text();
    assert!(to_haiku.contains("answer-one"), "{to_haiku}");
    assert!(!to_haiku.contains("sig-sonnet-1"), "{to_haiku}");
    let back_to_sonnet = requests[2].body_text();
    assert!(back_to_sonnet.contains("sig-sonnet-1"), "{back_to_sonnet}");
    assert!(!back_to_sonnet.contains("sig-haiku-1"), "{back_to_sonnet}");
    assert!(back_to_sonnet.contains("answer-two"), "{back_to_sonnet}");
    for request in &requests {
        assert!(!request.body_text().contains("produced_by"));
    }

    let producers: Vec<Value> = home
        .conversation()
        .iter()
        .filter(|line| line["role"] == "assistant")
        .map(|line| line["produced_by"].clone())
        .collect();
    assert_eq!(
        producers,
        vec![
            json!({"driver": ANTHROPIC, "model": SONNET}),
            json!({"driver": ANTHROPIC, "model": HAIKU}),
            json!({"driver": ANTHROPIC, "model": SONNET}),
        ]
    );
}

// ── S7: manifest refusals at run time ───────────────────────────────────────

/// Every alternates refusal stops `mur run` at load with `E-MAN-003`, naming the field.
#[test]
fn s7_each_refused_manifest_fails_mur_run_with_e_man_003_naming_the_field() {
    let home = Home::new();
    let openai = "http://127.0.0.1:9";
    let twice = format!("{GPT_ALTERNATE}{GPT_ALTERNATE}");
    let cases: Vec<(Spec<'_>, &str)> = vec![
        (
            Spec {
                openai: Some(openai),
                alternates: "    - name: primary\n      model: gpt-5\n      driver:\n        artifact: murmur-driver-openai\n",
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0].name",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: &twice,
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[1].name",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: "    - name: Gpt\n      model: gpt-5\n      driver:\n        artifact: murmur-driver-openai\n",
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0].name",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: "    - name: gpt\n      model: \"\"\n      driver:\n        artifact: murmur-driver-openai\n",
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0].model",
        ),
        (
            Spec {
                alternates: GPT_ALTERNATE,
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0].driver.artifact",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: "    - name: gpt\n      model: gpt-5\n      driver:\n        artifact: murmur-driver-openai\n        config:\n          store: true\n",
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0].driver.config",
        ),
        (
            Spec {
                alternates: "    - name: same\n      model: claude-sonnet-4-5\n      driver:\n        artifact: murmur-driver-anthropic\n",
                top: CONTROLLER,
                ..Spec::default()
            },
            "inference.alternates[0]",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: GPT_ALTERNATE,
                ..Spec::default()
            },
            "inference.alternates",
        ),
        (
            Spec {
                top: CONTROLLER,
                ..Spec::default()
            },
            "control.settings",
        ),
        (
            Spec {
                openai: Some(openai),
                alternates: GPT_ALTERNATE,
                top: "control:\n  agent_settings: [inference.max_tokens]\n",
                ..Spec::default()
            },
            "control.agent_settings",
        ),
    ];
    for (spec, field) in cases {
        let project = project("http://127.0.0.1:9", spec);
        let run = run_task(&home, &project);
        assert!(!run.ok, "{field}: {}", run.context());
        assert!(
            run.stderr.contains("E-MAN-003"),
            "{field}: {}",
            run.context()
        );
        assert!(
            run.stderr.contains(&format!("'{field}'")),
            "{field}: {}",
            run.context()
        );
    }

    // An alternate's driver without a gateway of its own.
    let project = project(
        "http://127.0.0.1:9",
        Spec {
            artifacts: "  - name: murmur-driver-openai\n    version: 0.1.0\n    runtime: driver\n",
            alternates: GPT_ALTERNATE,
            top: CONTROLLER,
            ..Spec::default()
        },
    );
    let run = run_task(&home, &project);
    assert!(run.stderr.contains("E-MAN-003"), "{}", run.context());
    assert!(
        run.stderr
            .contains("'inference.alternates[0].driver.artifact'"),
        "{}",
        run.context()
    );

    // Alternates under transport: process.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE}\nversion: 0.1.0\nartifacts:\n  - name: drv\n    version: 0.1.0\n    \
             runtime: driver\ninference:\n  transport: process\n  driver:\n    artifact: drv\n  \
             alternates:\n{GPT_ALTERNATE}"
        ),
    )
    .unwrap();
    let run = run_task(&home, &Project { dir });
    assert!(run.stderr.contains("E-MAN-003"), "{}", run.context());
    assert!(
        run.stderr.contains("'inference.alternates'"),
        "{}",
        run.context()
    );
}

// ── S8: metering ────────────────────────────────────────────────────────────

/// An alternate's gateway is metered like the primary's: the trace says so, no `W-SEC-030` is
/// printed, and a call on the alternate past the session ceiling ends `spend_ceiling_reached`
/// without reaching B.
#[test]
fn s8_an_alternates_gateway_is_metered_and_admitted_against_the_ceiling() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|_, _| anthropic_text("never"));
    let b = RecordingUpstream::with_handler(|_, _| openai_text("never"));
    let top = format!("{CONTROLLER}{THREADED}");
    let project = project(
        &a.endpoint,
        Spec {
            openai: Some(&b.endpoint),
            alternates: GPT_ALTERNATE,
            inference_extra: "  max_session_tokens: 20\n",
            top: &top,
            ..Spec::default()
        },
    );
    let capsule = Idle::start(&home, &project);
    assert!(set_driver(home.path(), "gpt").ok);
    capsule.task(1, "question");

    assert!(a.requests().is_empty());
    assert!(b.requests().is_empty(), "the refused call reached B");
    let events = capsule.events();
    let start = of_type(&events, "session_start")[0];
    let gateways = start["gateways"].as_array().unwrap();
    let openai = gateways
        .iter()
        .find(|gateway| gateway["artifact"] == OPENAI)
        .unwrap();
    assert_eq!(openai["metered"], true);
    assert_eq!(openai["credential_source"], "manifest");
    assert_eq!(
        start["inference_choices"],
        json!([
            {"name": "primary", "driver": ANTHROPIC, "model": SONNET, "credential_source": "manifest", "available": true},
            {"name": "gpt", "driver": OPENAI, "model": GPT, "credential_source": "manifest", "available": true},
        ])
    );
    assert_eq!(of_type(&events, "spend_ceiling_reached").len(), 1);
    let applied = of_type(&events, "control_applied");
    assert_eq!(applied[0]["value"], "gpt");
    let ended = of_type(&events, "task_end");
    assert_eq!(
        ended[0]["exit_status"], "spend_ceiling_reached",
        "{}",
        ended[0]
    );
    assert!(
        !capsule
            .stderr_once_it_holds("W-SEC-030")
            .contains("W-SEC-030"),
        "an alternate's gateway is metered"
    );
}

// ── S9: no alternates, no change ────────────────────────────────────────────

/// A capsule that declares no alternates writes nothing this feature introduces: no
/// `driver_choice`, no `inference_choices`, no `produced_by`, on the wire or on disk.
#[test]
fn s9_a_capsule_without_alternates_writes_nothing_new() {
    let home = Home::new();
    let a = RecordingUpstream::with_handler(|_, _| anthropic_text("plain"));
    let project = project(&a.endpoint, Spec::default());
    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let workdir = common::parse_workdir_from_stdout(&run.stdout);
    let trace = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
    for absent in [
        "driver_choice",
        "inference_choices",
        "produced_by",
        "switch-driver",
    ] {
        assert!(!trace.contains(absent), "{absent} in the trace");
    }
    let record = fs::read_to_string(
        home.path()
            .join(".murmur/conversations")
            .join(CAPSULE)
            .join(CONTEXT)
            .join("conversation.jsonl"),
    )
    .unwrap();
    assert!(!record.contains("produced_by"), "{record}");
    for request in a.requests() {
        assert!(!request.body_text().contains("produced_by"));
    }
    assert!(turns(&trace_events(&workdir))
        .iter()
        .all(|turn| turn.get("model").is_none()));
}
