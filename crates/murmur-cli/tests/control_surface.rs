//! The control plane: a capsule whose `murmur.yaml` declares `control:` can be changed while it
//! runs by whoever holds its session's control token, and by nothing else.
//!
//! Every capsule here is its own `mur run` process under a scratch `HOME`, because the token is
//! written to `$HOME/.murmur/running/` and `mur control` finds it only through the running record
//! there. Every assertion is about bytes that crossed a socket or a file a process wrote.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, TcpStream, UdpSocket},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use common::leaks::{files_containing, hex, leak_forms, sha256_hex};
use common::recording_upstream::{RecordingUpstream, Reply};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const TOOL: &str = "gateway-probe";
const VERSION: &str = "0.1.0";
const SECRET: &str = "CARD_TOKEN";
/// The driver's key, written literally: these tests are about the tool's credential.
const DRIVER_KEY: &str = "ctl-driver-key-5b1e";
const BEARER_AUTH: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
const UPSTREAM_REPLY: &str = r#"{"results":["gateway-probe"]}"#;
const IDLE_LIFECYCLE: &str = "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n";
const SETTINGS_AND_SECRET: &str =
    "control:\n  settings: [inference.max_tokens]\n  secrets: [CARD_TOKEN]\n";
const SETTINGS_ONLY: &str = "control:\n  settings: [inference.max_tokens]\n";

// ── homes, projects and commands ────────────────────────────────────────────

/// A scratch `$HOME` with the anthropic driver and the `gateway-probe` tool published into it.
struct Home {
    dir: TempDir,
    _artifacts: TempDir,
}

impl Home {
    fn new() -> Arc<Self> {
        let dir = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let driver = common::create_driver_artifact(
            artifacts.path(),
            DRIVER,
            VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&dir, &driver).success();
        let tool = common::create_tool_artifact_with_auth(
            artifacts.path(),
            TOOL,
            VERSION,
            &common::fixture_path("gateway-probe/tool/gateway-probe.wasm"),
            BEARER_AUTH,
        );
        common::publish_local(&dir, &tool).success();
        Arc::new(Self {
            dir,
            _artifacts: artifacts,
        })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn running_dir(&self) -> PathBuf {
        self.path().join(".murmur").join("running")
    }

    fn token_path(&self, session_id: &str) -> PathBuf {
        self.running_dir().join(format!("{session_id}.control"))
    }

    fn token(&self, session_id: &str) -> String {
        fs::read_to_string(self.token_path(session_id))
            .unwrap()
            .trim()
            .to_string()
    }

    /// The one `.control` file under the running directory, for a handler that has no session id.
    fn only_token_path(&self) -> PathBuf {
        let tokens: Vec<PathBuf> = fs::read_dir(self.running_dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "control"))
            .collect();
        assert_eq!(tokens.len(), 1, "{tokens:?}");
        tokens.into_iter().next().unwrap()
    }
}

fn mur(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .env_remove(SECRET);
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

/// Every `mur control` a case ran, so its output can be searched for what must not be in it.
type ControlLog = Arc<Mutex<Vec<Ran>>>;

/// `mur control <args>` against `home`, with `stdin` piped in when given.
fn mur_control(home: &Path, args: &[&str], stdin: Option<&[u8]>, log: &ControlLog) -> Ran {
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
    let ran = Ran::of(child.wait_with_output().unwrap());
    log.lock().unwrap().push(ran.clone());
    ran
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

/// An agent capsule on `provider`: `tool` is `gateway-probe`'s entry, when there is one, and
/// `extra` is spliced in at the top level.
fn project(provider: &str, tool: Option<&str>, extra: &str) -> Project {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: surface-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
             {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {provider}\n      \
             api_key: {DRIVER_KEY}\n{}inference:\n  transport: http\n  model: test-model\n  \
             max_tokens: 4096\n  driver:\n    artifact: {DRIVER}\n{extra}",
            tool.unwrap_or_default()
        ),
    )
    .unwrap();
    Project { dir }
}

/// `gateway-probe`'s entry, its gateway at `upstream` keyed by `${CARD_TOKEN}`.
fn keyed_tool(upstream: &str) -> String {
    format!(
        "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n    gateway:\n      \
         endpoint: {upstream}/v1\n      api_key: ${{{SECRET}}}\n"
    )
}

/// `gateway-probe`'s entry, its gateway at `upstream` and keyless.
fn keyless_tool(upstream: &str) -> String {
    format!(
        "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n    gateway:\n      \
         endpoint: {upstream}/v1\n      keyless: true\n"
    )
}

/// `mur run --task` on `project`, to the end of its one task.
fn run_task(home: &Home, project: &Project) -> Ran {
    Ran::of(
        mur(home.path())
            .args(["run", "--manifest"])
            .arg(project.manifest_path())
            .args(["--task", "call the tool", "--verbose"])
            .current_dir(project.dir.path())
            .output()
            .unwrap(),
    )
}

fn workdir_of(run: &Ran) -> PathBuf {
    common::parse_workdir_from_stdout(&run.stdout)
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

/// A marker no other case or run shares.
fn fresh_marker(label: &str) -> String {
    let seed = format!(
        "{label}-{}-{:?}-{:?}",
        std::process::id(),
        Instant::now(),
        thread::current().id()
    );
    format!("ctl-{label}-{}", &sha256_hex(seed.as_bytes())[..24])
}

// ── provider replies ────────────────────────────────────────────────────────

/// A turn calling `gateway-probe`, telling it (hex-encoded) which value to look for in its
/// environment.
fn probe_turn(id: &str, look_for: &str) -> String {
    tool_turn(id, TOOL, json!({"marker_hex": hex(look_for)}))
}

fn tool_turn(id: &str, name: &str, input: Value) -> String {
    json!({
        "id": format!("msg_{id}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": format!("toolu_{id}"), "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn request_json(request: &common::recording_upstream::RecordedRequest) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

// ── idle capsules ───────────────────────────────────────────────────────────

/// A queue+sleep capsule running as its own `mur run --json` process.
struct Idle {
    child: Child,
    startup: Value,
}

impl Idle {
    fn start(home: &Home, project: &Project, args: &[&str]) -> Self {
        let mut child = mur(home.path())
            .args(["run", "--manifest"])
            .arg(project.manifest_path())
            .arg("--json")
            .args(args)
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
        let stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
            }
        });
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(startup) => Self { child, startup },
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the capsule never printed where its door is");
            }
        }
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn port(&self) -> u16 {
        self.startup["url"]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn loopback(&self) -> String {
        format!("127.0.0.1:{}", self.port())
    }

    fn workdir(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap())
    }

    fn events(&self) -> Vec<Value> {
        trace_events(&self.workdir())
    }

    fn refusals(&self) -> Vec<Value> {
        of_type(&self.events(), "control_refused")
            .into_iter()
            .cloned()
            .collect()
    }
}

impl Drop for Idle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One HTTP response, read to the connection's close.
#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// One request to `addr` with exactly the head lines given, then `body`. `content-length` is the
/// body's own unless `headers` carries one.
fn http(addr: &str, method: &str, path: &str, headers: &[(&str, String)], body: &[u8]) -> Answer {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {text:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Answer {
        status,
        headers,
        body: body.to_string(),
    }
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

fn json_type() -> (&'static str, String) {
    ("Content-Type", "application/json".to_string())
}

fn put_setting(addr: &str, token: &str, value: &str) -> Answer {
    http(
        addr,
        "PUT",
        "/control/settings/inference.max_tokens",
        &[bearer(token), json_type()],
        format!("{{\"value\": {value}}}").as_bytes(),
    )
}

fn listing(addr: &str, token: &str) -> Value {
    let answer = http(addr, "GET", "/control", &[bearer(token)], b"");
    assert_eq!(answer.status, 200, "{answer:?}");
    answer.json()
}

/// No file under any of `roots`, no `mur` output in `ran`, and no body in `bodies` holds any form
/// of `secret` [`leak_forms`] names.
fn assert_recorded_nowhere(secret: &str, roots: &[&Path], ran: &[Ran], bodies: &[Vec<u8>]) {
    for form in leak_forms(secret) {
        for root in roots {
            let leaks = files_containing(root, form.as_bytes());
            assert!(
                leaks.is_empty(),
                "{form} under {}: {leaks:?}",
                root.display()
            );
        }
        for (index, run) in ran.iter().enumerate() {
            assert!(
                !run.holds(&form),
                "{form} in output {index}: {}",
                run.context()
            );
        }
        for (index, body) in bodies.iter().enumerate() {
            assert!(
                !body.windows(form.len()).any(|w| w == form.as_bytes()),
                "{form} in provider request {index}"
            );
        }
    }
}

// ── S1 ───────────────────────────────────────────────────────────────────────

/// A controller lowers `inference.max_tokens` while the capsule waits on its second inference
/// call. The first two calls carry the manifest's cap and the third the controller's, and the
/// trace records the change when it was accepted and again at the call that first used it.
#[test]
fn a_controller_changes_a_declared_setting_from_the_next_inference_call() {
    let home = Home::new();
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let log: ControlLog = Arc::default();
    let (handler_home, handler_log) = (Arc::clone(&home), Arc::clone(&log));
    let provider = RecordingUpstream::with_handler(move |index, _| {
        Reply::json(match index {
            0 => probe_turn("first", "unused"),
            1 => {
                let set = mur_control(
                    handler_home.path(),
                    &["set", "inference.max_tokens", "1234"],
                    None,
                    &handler_log,
                );
                assert!(set.ok, "{}", set.context());
                probe_turn("second", "unused")
            }
            _ => end_turn(),
        })
    });
    let project = project(
        &provider.endpoint,
        Some(&keyless_tool(&tool_upstream.endpoint)),
        SETTINGS_ONLY,
    );

    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let caps: Vec<Value> = provider
        .requests()
        .iter()
        .map(|request| request_json(request)["max_tokens"].clone())
        .collect();
    assert_eq!(caps, vec![json!(4096), json!(4096), json!(1234)]);

    let set = &log.lock().unwrap()[0];
    assert_eq!(
        set.stdout,
        "setting:  inference.max_tokens\nprevious: 4096\nvalue:    1234\napplies:  next inference call\n"
    );

    let workdir = workdir_of(&run);
    let events = trace_events(&workdir);
    let changes = of_type(&events, "control_change");
    assert_eq!(changes.len(), 1, "{changes:?}");
    let change = changes[0];
    assert_eq!(change["principal"], "controller");
    assert_eq!(change["kind"], "setting");
    assert_eq!(change["name"], "inference.max_tokens");
    assert_eq!(change["action"], "set");
    assert_eq!(change["previous"], 4096);
    assert_eq!(change["value"], 1234);
    assert_eq!(change["applies_from"], "next_inference_call");
    assert!(change["timestamp"].as_u64().is_some());

    let applied = of_type(&events, "control_applied");
    assert_eq!(applied.len(), 1, "{applied:?}");
    let applied = applied[0];
    // Zero-based, as every trace `turn` is: the third call is turn 2, and its `inference` line
    // carries the same number.
    assert_eq!(applied["turn"], 2);
    assert_eq!(applied["name"], "inference.max_tokens");
    assert_eq!(applied["value"], 1234);
    assert_eq!(applied["change_id"], change["event_id"]);
    let third_inference = of_type(&events, "inference")[2];
    assert_eq!(third_inference["turn"], applied["turn"]);
    let position = |id: &Value| events.iter().position(|e| e["event_id"] == *id).unwrap();
    assert!(position(&change["event_id"]) < position(&applied["event_id"]));
    assert!(position(&applied["event_id"]) < position(&third_inference["event_id"]));

    let show = Ran::of(
        mur(home.path())
            .args(["trace", "show"])
            .arg(workdir.join("trace.jsonl"))
            .current_dir(project.dir.path())
            .output()
            .unwrap(),
    );
    assert!(show.ok, "{}", show.context());
    assert!(
        show.stdout
            .contains("setting inference.max_tokens  4096 \u{2192} 1234  applied from turn 2"),
        "{}",
        show.context()
    );
}

// ── S2 ───────────────────────────────────────────────────────────────────────

/// A secret piped into `mur control secret` reaches the tool's upstream as its key, and no file,
/// output, provider request or record holds it in any encoding.
#[test]
fn an_injected_secret_reaches_the_gateway_and_no_record() {
    let home = Home::new();
    // The probe is told to look for the marker's first part, hex-encoded, so the marker's own hex
    // is written nowhere; an environment holding the marker necessarily holds its first part.
    let core = fresh_marker("inject");
    let marker = format!("{core}-tail");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let log: ControlLog = Arc::default();
    let (handler_home, handler_log) = (Arc::clone(&home), Arc::clone(&log));
    let (handler_marker, handler_core) = (marker.clone(), core.clone());
    let provider = RecordingUpstream::with_handler(move |index, _| {
        Reply::json(match index {
            0 => {
                let input = format!("{handler_marker}\n");
                let secret = mur_control(
                    handler_home.path(),
                    &["secret", SECRET],
                    Some(input.as_bytes()),
                    &handler_log,
                );
                assert!(secret.ok, "{}", secret.context());
                probe_turn("probe", &handler_core)
            }
            _ => end_turn(),
        })
    });
    let project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        SETTINGS_AND_SECRET,
    );

    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let upstream = tool_upstream.requests();
    assert_eq!(upstream.len(), 1);
    assert_eq!(
        upstream[0].header_values("authorization"),
        vec![format!("Bearer {marker}").as_str()]
    );

    let secret_run = log.lock().unwrap()[0].clone();
    assert_eq!(
        secret_run.stdout,
        format!("secret: {SECRET}\nset:    yes (new)\n")
    );

    let workdir = workdir_of(&run);
    let events = trace_events(&workdir);
    let gateways = &of_type(&events, "session_start")[0]["gateways"];
    let tool_gateway = gateways
        .as_array()
        .unwrap()
        .iter()
        .find(|gateway| gateway["artifact"] == TOOL)
        .unwrap();
    assert_eq!(tool_gateway["credential_source"], "injected");
    assert_eq!(
        of_type(&events, "session_start")[0]["control"],
        json!({"settings": ["inference.max_tokens"], "secrets": [SECRET]})
    );
    let change = of_type(&events, "control_change")[0];
    assert_eq!(change["kind"], "secret");
    assert_eq!(change["name"], SECRET);
    assert_eq!(change["action"], "set");
    assert_eq!(change["replaced"], false);
    assert!(change.get("value").is_none(), "{change}");
    assert!(change.get("previous").is_none(), "{change}");

    let requests = provider.requests();
    let report = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(report.contains("marker_in_env=false"), "{report}");
    assert!(report.contains("status=200"), "{report}");

    let bodies: Vec<Vec<u8>> = requests.iter().map(|r| r.body.clone()).collect();
    let mut ran = log.lock().unwrap().clone();
    ran.push(run.clone());
    assert_recorded_nowhere(
        &marker,
        &[home.path(), project.dir.path(), &workdir],
        &ran,
        &bodies,
    );
}

// ── S3 ───────────────────────────────────────────────────────────────────────

/// A keyed call made before any controller has injected the secret is refused inside the runtime
/// and never reaches the upstream; the next call, after injection, does.
#[test]
fn a_request_before_injection_never_reaches_the_upstream() {
    let home = Home::new();
    let marker = fresh_marker("late");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let log: ControlLog = Arc::default();
    let (handler_home, handler_log, handler_marker) =
        (Arc::clone(&home), Arc::clone(&log), marker.clone());
    let provider = RecordingUpstream::with_handler(move |index, _| {
        Reply::json(match index {
            0 => probe_turn("before", "unused"),
            1 => {
                let input = format!("{handler_marker}\n");
                let secret = mur_control(
                    handler_home.path(),
                    &["secret", SECRET],
                    Some(input.as_bytes()),
                    &handler_log,
                );
                assert!(secret.ok, "{}", secret.context());
                probe_turn("after", "unused")
            }
            _ => end_turn(),
        })
    });
    let project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        SETTINGS_AND_SECRET,
    );

    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let upstream = tool_upstream.requests();
    assert_eq!(
        upstream.len(),
        1,
        "the call before injection reached the upstream"
    );
    assert_eq!(
        upstream[0].header_values("authorization"),
        vec![format!("Bearer {marker}").as_str()]
    );

    let first_result = String::from_utf8_lossy(&provider.requests()[1].body).into_owned();
    assert!(first_result.contains(SECRET), "{first_result}");
    assert!(first_result.contains("not been injected"), "{first_result}");

    let events = trace_events(&workdir_of(&run));
    let credential_events = of_type(&events, "gateway_credential");
    assert_eq!(credential_events.len(), 1, "{credential_events:?}");
    let event = credential_events[0];
    assert_eq!(event["artifact"], TOOL);
    assert_eq!(event["source"], "injected");
    assert_eq!(event["credential"], SECRET);
    assert_eq!(event["change"], "unreadable");
    assert_eq!(event["reason"], "not_injected");
}

// ── S4 ───────────────────────────────────────────────────────────────────────

/// Nothing but this session's own token, exactly as minted, authenticates: every other caller is
/// answered `401` with the Bearer challenge before its body is read, nothing changes, and each
/// refusal is recorded without naming anything.
#[test]
fn an_untrusted_caller_cannot_change_anything() {
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let extra = format!("{IDLE_LIFECYCLE}{SETTINGS_AND_SECRET}");
    let target_project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        &extra,
    );
    let other_project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        &extra,
    );
    let target = Idle::start(&home, &target_project, &[]);
    let other = Idle::start(&home, &other_project, &[]);
    let token = home.token(&target.session_id());
    let other_token = home.token(&other.session_id());
    assert_ne!(token, other_token);
    let marker = fresh_marker("untrusted");

    let swapped_case: String = token
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    let addr = target.loopback();
    let attempts: Vec<Answer> = vec![
        http(
            &addr,
            "PUT",
            "/control/settings/inference.max_tokens",
            &[json_type()],
            br#"{"value": 1}"#,
        ),
        put_setting(&addr, "garbage", "1"),
        put_setting(&addr, &other_token, "1"),
        put_setting(&addr, &swapped_case, "1"),
        http(
            &addr,
            "PUT",
            "/control/secrets/CARD_TOKEN",
            &[bearer(&other_token)],
            marker.as_bytes(),
        ),
    ];
    for answer in &attempts {
        assert_eq!(answer.status, 401, "{answer:?}");
        assert_eq!(
            answer.header("www-authenticate"),
            Some("Bearer realm=\"murmur-control\"")
        );
        assert!(!answer.body.contains("inference.max_tokens"));
        assert!(!answer.body.contains(SECRET));
    }

    assert_eq!(
        listing(&addr, &token),
        json!({
            "session_id": target.session_id(),
            "settings": [{"name": "inference.max_tokens", "value": 4096}],
            "secrets": [{"name": SECRET, "set": false}],
        })
    );

    let refusals = target.refusals();
    assert_eq!(refusals.len(), attempts.len(), "{refusals:?}");
    for refusal in &refusals {
        assert_eq!(refusal["status"], 401);
        assert_eq!(refusal["reason"], "unauthenticated");
        assert!(refusal.get("name").is_none(), "{refusal}");
        assert!(refusal.get("token_id").is_none(), "{refusal}");
    }
    assert!(of_type(&target.events(), "control_change").is_empty());
    assert!(other.refusals().is_empty());

    let target_workdir = target.workdir();
    let other_workdir = other.workdir();
    assert_recorded_nowhere(
        &marker,
        &[
            home.path(),
            target_project.dir.path(),
            other_project.dir.path(),
            &target_workdir,
            &other_workdir,
        ],
        &[],
        &[],
    );
}

// ── S5 ───────────────────────────────────────────────────────────────────────

/// An authenticated controller is still refused anything the manifest does not declare, any
/// method a resource does not answer, and any value the setting or secret cannot take — each
/// refusal recorded with its status and the name it asked for, and none of them changing anything.
#[test]
fn a_controller_is_refused_what_is_not_declared() {
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        &format!("{IDLE_LIFECYCLE}{SETTINGS_AND_SECRET}"),
    );
    let capsule = Idle::start(&home, &project, &[]);
    let token = home.token(&capsule.session_id());
    let addr = capsule.loopback();
    let marker = fresh_marker("refused");

    let undeclared_setting = |name: &str| {
        http(
            &addr,
            "PUT",
            &format!("/control/settings/{name}"),
            &[bearer(&token), json_type()],
            br#"{"value": 2048}"#,
        )
    };
    let secret_put = |body: &[u8], extra: &[(&str, String)]| {
        let mut headers = vec![bearer(&token)];
        headers.extend(extra.iter().cloned());
        http(&addr, "PUT", "/control/secrets/CARD_TOKEN", &headers, body)
    };

    let mut expected: Vec<(u16, Option<&str>)> = Vec::new();
    let mut check = |answer: Answer, status: u16, name: Option<&'static str>| {
        assert_eq!(answer.status, status, "{answer:?}");
        assert!(answer.json()["error"].as_str().is_some(), "{answer:?}");
        assert!(!answer.body.contains(&marker), "{answer:?}");
        expected.push((status, name));
    };

    check(
        undeclared_setting("inference.model"),
        404,
        Some("inference.model"),
    );
    check(
        undeclared_setting("context.max_tokens"),
        404,
        Some("context.max_tokens"),
    );
    check(
        http(
            &addr,
            "PUT",
            "/control/secrets/OTHER_TOKEN",
            &[bearer(&token)],
            marker.as_bytes(),
        ),
        404,
        Some("OTHER_TOKEN"),
    );
    check(
        http(
            &addr,
            "GET",
            "/control/nothing-here",
            &[bearer(&token)],
            b"",
        ),
        404,
        None,
    );
    let wrong_method = http(
        &addr,
        "POST",
        "/control/settings/inference.max_tokens",
        &[bearer(&token), json_type()],
        br#"{"value": 2048}"#,
    );
    assert_eq!(wrong_method.header("allow"), Some("PUT"));
    check(wrong_method, 405, Some("inference.max_tokens"));
    for value in ["0", "-1", "\"2048\"", "4294967296"] {
        check(
            put_setting(&addr, &token, value),
            422,
            Some("inference.max_tokens"),
        );
    }
    check(
        http(
            &addr,
            "PUT",
            "/control/settings/inference.max_tokens",
            &[bearer(&token)],
            br#"{"value": 2048}"#,
        ),
        415,
        Some("inference.max_tokens"),
    );
    // Declared longer than the cap and never sent: an answer at all proves the body went unread.
    check(
        secret_put(b"", &[("Content-Length", "8193".to_string())]),
        413,
        Some(SECRET),
    );
    check(secret_put(b"", &[]), 422, Some(SECRET));
    for bad in [
        format!("{marker}\r"),
        format!("{marker}\nsecond"),
        format!("{marker}\0"),
    ] {
        check(secret_put(bad.as_bytes(), &[]), 422, Some(SECRET));
    }

    assert_eq!(
        listing(&addr, &token),
        json!({
            "session_id": capsule.session_id(),
            "settings": [{"name": "inference.max_tokens", "value": 4096}],
            "secrets": [{"name": SECRET, "set": false}],
        })
    );
    let recorded: Vec<(u16, Option<String>)> = capsule
        .refusals()
        .iter()
        .map(|refusal| {
            (
                refusal["status"].as_u64().unwrap() as u16,
                refusal["name"].as_str().map(str::to_string),
            )
        })
        .collect();
    let expected: Vec<(u16, Option<String>)> = expected
        .into_iter()
        .map(|(status, name)| (status, name.map(str::to_string)))
        .collect();
    assert_eq!(recorded, expected);
    assert!(of_type(&capsule.events(), "control_change").is_empty());
    assert_recorded_nowhere(
        &marker,
        &[home.path(), project.dir.path(), &capsule.workdir()],
        &[],
        &[],
    );
}

// ── S6 ───────────────────────────────────────────────────────────────────────

/// This host's own non-loopback IPv4 address, when it has one: the source address a connection
/// to it arrives from.
fn non_loopback_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    // Connecting a UDP socket sends nothing; it only selects the route and the source address.
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
        _ => None,
    }
}

/// On a capsule bound to every interface, a secret is set or forgotten only over loopback; the
/// same requests over the host's LAN address are refused and recorded, while a setting may
/// still be changed from there.
#[test]
fn a_secret_is_accepted_only_over_loopback() {
    let Some(lan) = non_loopback_ipv4() else {
        eprintln!("a_secret_is_accepted_only_over_loopback: skipped, this host has no non-loopback IPv4 address");
        return;
    };
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        &format!("{IDLE_LIFECYCLE}{SETTINGS_AND_SECRET}"),
    );
    let capsule = Idle::start(&home, &project, &["--bind", "0.0.0.0"]);
    let token = home.token(&capsule.session_id());
    let local = capsule.loopback();
    let remote = format!("{lan}:{}", capsule.port());
    let secret_set = |addr: &str, value: &str| {
        http(
            addr,
            "PUT",
            "/control/secrets/CARD_TOKEN",
            &[bearer(&token)],
            value.as_bytes(),
        )
    };
    let secret_forget = |addr: &str| {
        http(
            addr,
            "DELETE",
            "/control/secrets/CARD_TOKEN",
            &[bearer(&token)],
            b"",
        )
    };
    let is_set = || listing(&local, &token)["secrets"][0]["set"].clone();

    assert_eq!(secret_set(&local, &fresh_marker("local")).status, 200);
    assert_eq!(is_set(), true);
    let refused = secret_set(&remote, &fresh_marker("remote"));
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(is_set(), true);
    assert_eq!(secret_forget(&remote).status, 403);
    assert_eq!(is_set(), true);
    assert_eq!(secret_forget(&local).status, 200);
    assert_eq!(is_set(), false);

    let setting = put_setting(&remote, &token, "2048");
    assert_eq!(setting.status, 200, "{setting:?}");
    assert_eq!(listing(&local, &token)["settings"][0]["value"], json!(2048));

    let refusals = capsule.refusals();
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    for refusal in refusals {
        assert_eq!(refusal["status"], 403);
        assert_eq!(refusal["reason"], "not_loopback");
        assert_eq!(refusal["name"], SECRET);
    }
}

// ── S7 ───────────────────────────────────────────────────────────────────────

/// A secret replaced between two calls reaches the second with its new value, and once forgotten
/// the next call never leaves the runtime.
#[test]
fn a_secret_can_be_replaced_and_forgotten() {
    let home = Home::new();
    let first = fresh_marker("first");
    let second = fresh_marker("second");
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let log: ControlLog = Arc::default();
    let (handler_home, handler_log) = (Arc::clone(&home), Arc::clone(&log));
    let (handler_first, handler_second) = (first.clone(), second.clone());
    let provider = RecordingUpstream::with_handler(move |index, _| {
        let control = |args: &[&str], stdin: Option<String>| {
            let ran = mur_control(
                handler_home.path(),
                args,
                stdin.as_deref().map(str::as_bytes),
                &handler_log,
            );
            assert!(ran.ok, "{}", ran.context());
        };
        Reply::json(match index {
            0 => {
                control(&["secret", SECRET], Some(format!("{handler_first}\n")));
                probe_turn("one", "unused")
            }
            1 => {
                control(&["secret", SECRET], Some(format!("{handler_second}\r\n")));
                probe_turn("two", "unused")
            }
            2 => {
                control(&["forget", SECRET], None);
                probe_turn("three", "unused")
            }
            _ => end_turn(),
        })
    });
    let project = project(
        &provider.endpoint,
        Some(&keyed_tool(&tool_upstream.endpoint)),
        SETTINGS_AND_SECRET,
    );

    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let upstream = tool_upstream.requests();
    assert_eq!(
        upstream.len(),
        2,
        "the call after the forget reached the upstream"
    );
    assert_eq!(
        upstream[0].header_values("authorization"),
        vec![format!("Bearer {first}").as_str()]
    );
    assert_eq!(
        upstream[1].header_values("authorization"),
        vec![format!("Bearer {second}").as_str()]
    );

    let outputs: Vec<String> = log
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.stdout.clone())
        .collect();
    assert_eq!(
        outputs,
        vec![
            format!("secret: {SECRET}\nset:    yes (new)\n"),
            format!("secret: {SECRET}\nset:    yes (replaced)\n"),
            format!("secret: {SECRET}\nset:    no\n"),
        ]
    );

    let workdir = workdir_of(&run);
    let events = trace_events(&workdir);
    let sequence: Vec<Value> = events
        .iter()
        .filter(|e| e["event_type"] == "control_change" || e["event_type"] == "gateway_credential")
        .map(|e| {
            json!({
                "event_type": e["event_type"],
                "action": e.get("action"),
                "replaced": e.get("replaced"),
                "change": e.get("change"),
                "reason": e.get("reason"),
            })
        })
        .collect();
    let step = |event_type: &str, action: Value, replaced: Value, change: Value, reason: Value| json!({"event_type": event_type, "action": action, "replaced": replaced, "change": change, "reason": reason});
    assert_eq!(
        sequence,
        vec![
            step(
                "control_change",
                json!("set"),
                json!(false),
                Value::Null,
                Value::Null
            ),
            step(
                "control_change",
                json!("set"),
                json!(true),
                Value::Null,
                Value::Null
            ),
            step(
                "control_change",
                json!("forget"),
                Value::Null,
                Value::Null,
                Value::Null
            ),
            step(
                "gateway_credential",
                Value::Null,
                Value::Null,
                json!("unreadable"),
                json!("not_injected")
            ),
        ]
    );

    let bodies: Vec<Vec<u8>> = provider.requests().iter().map(|r| r.body.clone()).collect();
    let mut ran = log.lock().unwrap().clone();
    ran.push(run.clone());
    for marker in [&first, &second] {
        assert_recorded_nowhere(
            marker,
            &[home.path(), project.dir.path(), &workdir],
            &ran,
            &bodies,
        );
    }
}

// ── S8 ───────────────────────────────────────────────────────────────────────

/// A capsule with no `control:` block has no token and no surface: every request under
/// `/control` is `404` whatever it carries, `mur control` says so, and its agent card is the
/// controlled capsule's but for identity.
#[test]
fn no_control_block_means_no_surface() {
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    let plain_project = project(&provider.endpoint, None, IDLE_LIFECYCLE);
    let controlled_project = project(
        &provider.endpoint,
        None,
        &format!("{IDLE_LIFECYCLE}{SETTINGS_ONLY}"),
    );
    let plain = Idle::start(&home, &plain_project, &[]);
    let controlled = Idle::start(&home, &controlled_project, &[]);
    assert!(!home.token_path(&plain.session_id()).exists());
    let controlled_token = home.token(&controlled.session_id());

    let addr = plain.loopback();
    for (method, path, headers, body) in [
        ("GET", "/control", vec![], &b""[..]),
        ("GET", "/control", vec![bearer(&controlled_token)], &b""[..]),
        ("GET", "/control", vec![bearer("garbage")], &b""[..]),
        (
            "PUT",
            "/control/settings/inference.max_tokens",
            vec![bearer(&controlled_token), json_type()],
            &br#"{"value": 5}"#[..],
        ),
        (
            "PUT",
            "/control/secrets/CARD_TOKEN",
            vec![bearer(&controlled_token)],
            &b"value"[..],
        ),
        ("DELETE", "/control/secrets/CARD_TOKEN", vec![], &b""[..]),
        ("GET", "/control/anything", vec![], &b""[..]),
    ] {
        let answer = http(&addr, method, path, &headers, body);
        assert_eq!(answer.status, 404, "{method} {path}: {answer:?}");
    }
    assert!(of_type(&plain.events(), "control_refused").is_empty());

    let log: ControlLog = Arc::default();
    let session = plain.session_id();
    for args in [
        vec!["show", session.as_str()],
        vec!["set", "inference.max_tokens", "5", session.as_str()],
    ] {
        let ran = mur_control(home.path(), &args, None, &log);
        assert!(!ran.ok, "{}", ran.context());
        assert!(ran.stderr.contains("E-RUN-042"), "{}", ran.context());
    }

    let card = |capsule: &Idle| {
        let answer = http(
            &capsule.loopback(),
            "GET",
            "/.well-known/agent-card.json",
            &[],
            b"",
        );
        assert_eq!(answer.status, 200);
        answer
            .body
            .replace(&capsule.session_id(), "<session>")
            .replace(&capsule.port().to_string(), "<port>")
    };
    let (plain_card, controlled_card) = (card(&plain), card(&controlled));
    assert_eq!(plain_card, controlled_card);
    assert!(!controlled_card.to_ascii_lowercase().contains("control"));
}

// ── S9 ───────────────────────────────────────────────────────────────────────

/// The control token is handed to nothing inside the capsule: a tool's environment does not hold
/// it, no provider request, record or trace line carries it, a shell subprocess on a Landlock
/// host cannot read the file it is in, and the capsule's scope names nothing under `~/.murmur`.
#[test]
fn the_control_token_is_unreachable_from_inside_the_capsule() {
    let home = Home::new();
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let log: ControlLog = Arc::default();
    let token_seen: Arc<Mutex<Option<(PathBuf, String)>>> = Arc::default();
    let (handler_home, handler_log, handler_token) =
        (Arc::clone(&home), Arc::clone(&log), Arc::clone(&token_seen));

    // Which of the two cases runs is decided before launch, from the same probe the session uses.
    let probe_project = project("http://127.0.0.1:9", None, "");
    let scope = common::explain_scope_json(&home.dir, &probe_project.manifest_path());
    let landlock = scope["enforcement_tier"]
        .as_str()
        .is_some_and(|tier| tier.contains("landlock"));

    let provider = RecordingUpstream::with_handler(move |index, _| {
        Reply::json(match index {
            0 => {
                let path = handler_home.only_token_path();
                let token = fs::read_to_string(&path).unwrap().trim().to_string();
                *handler_token.lock().unwrap() = Some((path, token.clone()));
                let set = mur_control(
                    handler_home.path(),
                    &["set", "inference.max_tokens", "2048"],
                    None,
                    &handler_log,
                );
                assert!(set.ok, "{}", set.context());
                probe_turn("probe", &token)
            }
            1 if landlock => {
                let (path, _) = handler_token.lock().unwrap().clone().unwrap();
                tool_turn(
                    "cat",
                    "bash",
                    json!({"command": format!("cat {}", path.display())}),
                )
            }
            _ => end_turn(),
        })
    });
    let extra =
        format!("capabilities:\n  shell:\n    allow:\n      - bash\n      - cat\n{SETTINGS_ONLY}");
    let project = project(
        &provider.endpoint,
        Some(&keyless_tool(&tool_upstream.endpoint)),
        &extra,
    );

    let scope = common::explain_scope_json(&home.dir, &project.manifest_path()).to_string();
    let murmur_home = home.path().join(".murmur");
    assert!(
        !scope.contains(murmur_home.to_str().unwrap()),
        "the scope names a path under ~/.murmur: {scope}"
    );

    let run = run_task(&home, &project);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());
    let (_, token) = token_seen
        .lock()
        .unwrap()
        .clone()
        .expect("the handler read the token");

    let requests = provider.requests();
    let report = String::from_utf8_lossy(&requests[1].body).into_owned();
    assert!(report.contains("marker_in_env=false"), "{report}");
    for (index, request) in requests.iter().enumerate() {
        assert!(
            !String::from_utf8_lossy(&request.body).contains(&token),
            "provider request {index} carries the token"
        );
    }

    let workdir = workdir_of(&run);
    let trace = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
    assert!(!trace.contains(&token));
    let events = trace_events(&workdir);
    let change = of_type(&events, "control_change")[0];
    assert_eq!(
        change["token_id"],
        json!(&sha256_hex(token.as_bytes())[..16])
    );

    // The session is over, so its token file is gone and nothing else under HOME — the
    // conversation record included — may hold the token.
    for root in [home.path(), project.dir.path(), workdir.as_path()] {
        let leaks = files_containing(root, token.as_bytes());
        assert!(leaks.is_empty(), "{}: {leaks:?}", root.display());
    }
    assert!(!run.holds(&token));

    if landlock {
        let cat_result = String::from_utf8_lossy(&requests[2].body).into_owned();
        assert!(
            cat_result.contains("Permission denied"),
            "the shell read the token file: {cat_result}"
        );
    } else {
        eprintln!(
            "the_control_token_is_unreachable_from_inside_the_capsule: the Landlock half skipped, \
             this host's enforcement tier is {}",
            scope
        );
    }
}

// ── S10 ──────────────────────────────────────────────────────────────────────

/// The token file is owner-only while the session runs and gone once it is stopped, and a token
/// saved from that session does not open a new session of the same capsule on the same port.
#[test]
fn the_control_token_lives_as_long_as_the_session() {
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    let port = common::free_port();
    let project = project(
        &provider.endpoint,
        None,
        &format!("{IDLE_LIFECYCLE}{SETTINGS_ONLY}network:\n  internal_port: {port}\n"),
    );

    let first = Idle::start(&home, &project, &[]);
    assert_eq!(first.port(), port);
    let token_path = home.token_path(&first.session_id());
    let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let saved = home.token(&first.session_id());
    assert!(saved.starts_with("ctl1."), "unexpected token shape");
    assert_eq!(put_setting(&first.loopback(), &saved, "2048").status, 200);

    let stop = Ran::of(mur(home.path()).args(["stop", "@1"]).output().unwrap());
    assert!(stop.ok, "{}", stop.context());
    let deadline = Instant::now() + Duration::from_secs(20);
    while token_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!token_path.exists(), "the token outlived its session");
    assert!(!home
        .running_dir()
        .join(format!("{}.json", first.session_id()))
        .exists());
    drop(first);

    let second = Idle::start(&home, &project, &[]);
    assert_eq!(second.port(), port);
    let refused = put_setting(&second.loopback(), &saved, "1024");
    assert_eq!(refused.status, 401, "{refused:?}");
    assert_eq!(
        listing(&second.loopback(), &home.token(&second.session_id()))["settings"][0]["value"],
        json!(4096)
    );
}

// ── E-RUN-041 ────────────────────────────────────────────────────────────────

/// A capsule that declares `control:` and cannot write its token beside its running record does
/// not run: a controllability no controller could reach is refused rather than dropped.
#[test]
fn a_capsule_that_cannot_write_its_control_token_refuses_to_launch() {
    let home = Home::new();
    let provider = RecordingUpstream::replying("{}");
    fs::create_dir_all(home.path().join(".murmur")).unwrap();
    fs::write(home.running_dir(), "not a directory").unwrap();
    let project = project(&provider.endpoint, None, SETTINGS_ONLY);

    let run = run_task(&home, &project);
    assert!(!run.ok, "{}", run.context());
    assert!(run.stderr.contains("E-RUN-041"), "{}", run.context());
    assert!(provider.requests().is_empty());
}

// ── S11 (CLI half) ───────────────────────────────────────────────────────────

/// `mur doctor` names a `control.secrets` credential as one a controller supplies, and never as a
/// variable this workspace is missing.
#[test]
fn doctor_does_not_report_a_control_secret_as_missing() {
    let home = Home::new();
    let project = project(
        "http://127.0.0.1:9",
        Some(&keyed_tool("https://cards.example.com")),
        SETTINGS_AND_SECRET,
    );
    let doctor = Ran::of(
        mur(home.path())
            .arg("doctor")
            .current_dir(project.dir.path())
            .output()
            .unwrap(),
    );
    assert!(
        doctor.stdout.contains("Control secrets"),
        "{}",
        doctor.context()
    );
    assert!(
        doctor
            .stdout
            .contains("CARD_TOKEN   supplied by a controller at run time"),
        "{}",
        doctor.context()
    );
    assert!(
        !doctor
            .stdout
            .lines()
            .any(|line| line.contains("CARD_TOKEN") && line.contains("unset")),
        "{}",
        doctor.context()
    );
}
