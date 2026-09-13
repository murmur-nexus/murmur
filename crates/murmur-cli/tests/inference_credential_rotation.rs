//! Inference credential rotation: `mur config set -g credentials.NAME <key>` reaches a capsule that
//! is already running on its next inference request, a rejected key is re-read and retried once,
//! and a rejection that persists fails the session naming where the key came from.
//!
//! Driven through the real `mur` binary against a loopback upstream the test controls: it records
//! each request's `x-api-key`, can hold a reply until released, and picks each reply's status.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const SKILL: &str = "rotation-skill";
const VERSION: &str = "0.1.0";
const AUTH: &str = "inference_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const NAME: &str = "ROTATION_TEST_KEY";
const OLD: &str = "sk-rotation-old-7a41c9e2";
const NEW: &str = "sk-rotation-new-3d82e0b5";
const NEW2: &str = "sk-rotation-newer-b61f07d4";
const LITERAL: &str = "sk-rotation-literal-c9e5a310";
const W_SEC_026_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-026";
const WAIT: Duration = Duration::from_secs(120);

const TOOL_USE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"test-model","content":[{"type":"tool_use","id":"toolu_skill","name":"rotation-skill","input":{}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#;
const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
const UNAUTHORIZED: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;

/// The label a recorded key is printed under, so no test output carries a key.
fn label(key: &str) -> &'static str {
    match key {
        OLD => "old",
        NEW => "new",
        NEW2 => "newer",
        LITERAL => "literal",
        _ => "other",
    }
}

#[derive(Clone)]
struct Recorded {
    keys: Vec<String>,
    body: Vec<u8>,
    arrived: Instant,
}

impl Recorded {
    fn key(&self) -> &str {
        assert_eq!(self.keys.len(), 1, "exactly one x-api-key per request");
        &self.keys[0]
    }
}

type Respond = dyn Fn(usize, &Recorded) -> (u16, &'static str) + Send + Sync;

struct Shared {
    requests: Vec<Recorded>,
    released: HashSet<usize>,
}

struct Upstream {
    endpoint: String,
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

impl Upstream {
    /// Answers request `n` (zero-based) with `respond(n, request)`, after `release(n)` when `n` is
    /// in `hold`.
    fn start(
        hold: &[usize],
        respond: impl Fn(usize, &Recorded) -> (u16, &'static str) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new((
            Mutex::new(Shared {
                requests: Vec::new(),
                released: HashSet::new(),
            }),
            Condvar::new(),
        ));
        let hold: HashSet<usize> = hold.iter().copied().collect();
        let respond: Arc<Respond> = Arc::new(respond);
        let thread_shared = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let (lock, condvar) = &*thread_shared;
                let index = {
                    let mut shared = lock.lock().unwrap();
                    shared.requests.push(request.clone());
                    condvar.notify_all();
                    shared.requests.len() - 1
                };
                if hold.contains(&index) {
                    let mut shared = lock.lock().unwrap();
                    while !shared.released.contains(&index) {
                        shared = condvar.wait(shared).unwrap();
                    }
                }
                let (status, body) = respond(index, &request);
                let reason = if status == 200 { "OK" } else { "Unauthorized" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });
        Self { endpoint, shared }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.shared.0.lock().unwrap().requests.clone()
    }

    fn wait_for(&self, count: usize) {
        let (lock, condvar) = &*self.shared;
        let deadline = Instant::now() + WAIT;
        let mut shared = lock.lock().unwrap();
        while shared.requests.len() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "upstream saw fewer than {count} requests");
            shared = condvar.wait_timeout(shared, left).unwrap().0;
        }
    }

    fn release(&self, index: usize) {
        let (lock, condvar) = &*self.shared;
        lock.lock().unwrap().released.insert(index);
        condvar.notify_all();
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let arrived = Instant::now();
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let headers: Vec<(String, String)> = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Recorded {
        keys: headers
            .into_iter()
            .filter(|(n, _)| n == "x-api-key")
            .map(|(_, v)| v)
            .collect(),
        body,
        arrived,
    })
}

enum ApiKey {
    Reference,
    Literal,
}

struct Capsule {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

impl Capsule {
    fn new(upstream: &Upstream, api_key: ApiKey, extra: &str) -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let driver = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            AUTH,
        );
        common::publish_local(&home, &driver).success();
        let skill = common::create_skill_artifact(
            artifacts.path(),
            SKILL,
            VERSION,
            "# Rotation skill\nAnswer briefly.",
        );
        common::publish_local(&home, &skill).success();
        let api_key = match api_key {
            ApiKey::Reference => format!("${{{NAME}}}"),
            ApiKey::Literal => LITERAL.to_string(),
        };
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: rotation-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {VERSION}\n    runtime: driver\n  - name: {SKILL}\n    version: \
                 {VERSION}\n    runtime: skill\n{extra}inference:\n  transport: http\n  \
                 endpoint: {}\n  model: test-model\n  api_key: {api_key}\n  driver:\n    \
                 artifact: {DRIVER}\n",
                upstream.endpoint
            ),
        )
        .unwrap();
        Self {
            home,
            project,
            manifest,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".murmur").join("config.yaml")
    }

    /// `mur` with a scratch `HOME`, run from the project, and `NAME` present only when `env_key`
    /// gives it a value.
    fn mur(&self, env_key: Option<&str>) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(NAME)
            .current_dir(self.project.path());
        if let Some(key) = env_key {
            command.env(NAME, key);
        }
        command
    }

    /// `mur config set -g credentials.NAME <value>`, waited for. Returns when it exited.
    fn set_credential(&self, value: &str) -> (Output, Instant) {
        let output = self
            .mur(None)
            .args(["config", "set", "-g", &format!("credentials.{NAME}"), value])
            .output()
            .unwrap();
        let exited = Instant::now();
        assert!(
            output.status.success(),
            "config set failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (output, exited)
    }

    fn spawn_run(&self, env_key: Option<&str>) -> std::process::Child {
        self.mur(env_key)
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                "Say hello.",
                "--verbose",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, env_key: Option<&str>) -> Run {
        Run::of(self.spawn_run(env_key).wait_with_output().unwrap())
    }

    fn explain_scope(&self, env_key: Option<&str>) -> Run {
        Run::of(
            self.mur(env_key)
                .args([
                    "run",
                    "--manifest",
                    self.manifest.to_str().unwrap(),
                    "--explain-scope",
                ])
                .output()
                .unwrap(),
        )
    }

    fn doctor(&self, env_key: Option<&str>) -> Run {
        Run::of(self.mur(env_key).arg("doctor").output().unwrap())
    }
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn ok(&self) -> bool {
        self.stdout.contains("status:  ok")
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir().join("out/result.txt")).unwrap_or_else(|err| {
            panic!(
                "no out/result.txt ({err}); stdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.workdir().join("trace.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn credential_events(&self) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == "inference_credential")
            .collect()
    }

    fn session_start(&self) -> Value {
        self.trace()
            .into_iter()
            .find(|event| event["event_type"] == "session_start")
            .expect("session_start")
    }

    fn lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(code))
            .collect()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

fn files_containing(root: &Path, needle: &[u8], skip: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == skip {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if fs::read(&path)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

fn directories_named_like(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name().to_string_lossy().starts_with(prefix) {
                    found.push(path.clone());
                }
                stack.push(path);
            }
        }
    }
    found
}

/// What the S1 flow leaves behind for S1 and S6 to assert on.
struct RotationFlow {
    capsule: Capsule,
    upstream: Upstream,
    run: Run,
    set_outputs: Vec<Output>,
    set_exited: Instant,
}

/// The config holds `OLD`; request 1 is held until `NEW` has been written; request 1 answers with a
/// `tool_use` for the capsule's skill, so a second request follows.
fn rotation_flow(extra: &str) -> RotationFlow {
    let upstream = Upstream::start(&[0], |index, _| {
        (200, if index == 0 { TOOL_USE } else { END_TURN })
    });
    let capsule = Capsule::new(&upstream, ApiKey::Reference, extra);
    let (first_set, _) = capsule.set_credential(OLD);

    let child = capsule.spawn_run(None);
    upstream.wait_for(1);
    let (second_set, set_exited) = capsule.set_credential(NEW);
    upstream.release(0);
    let run = Run::of(child.wait_with_output().unwrap());

    RotationFlow {
        capsule,
        upstream,
        run,
        set_outputs: vec![first_set, second_set],
        set_exited,
    }
}

/// S1: a key replaced with `mur config set -g` while the capsule waits on its first reply is the
/// key its second request carries.
#[test]
fn rotation_reaches_a_running_capsule() {
    let flow = rotation_flow("");
    let run = &flow.run;
    assert!(run.ok(), "{}", run.context());

    let requests = flow.upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.context());
    for (index, request) in requests.iter().enumerate() {
        println!("request {} x-api-key: {}", index + 1, label(request.key()));
    }
    assert_eq!(requests[0].key(), OLD);
    assert_eq!(requests[1].key(), NEW);
    assert!(requests
        .iter()
        .filter(|request| request.arrived > flow.set_exited)
        .all(|request| request.key() != OLD));
    // The skill call was granted and answered, so the second turn is the model reading its result.
    assert!(
        String::from_utf8_lossy(&requests[1].body).contains("Rotation skill"),
        "the skill's guidance reached the second turn"
    );

    let delay = requests[1].arrived.duration_since(flow.set_exited);
    println!(
        "config set exit -> request 2 arrival: {} ms",
        delay.as_millis()
    );

    let events = run.credential_events();
    println!("inference_credential events: {events:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["change"], "rotated");
    assert_eq!(events[0]["trigger"], "file_changed");
    assert_eq!(events[0]["source"], "config");
    assert_eq!(events[0]["credential"], NAME);
    assert_eq!(
        events[0]["parent_id"],
        run.session_start()["event_id"],
        "parented to the session node"
    );
    assert_eq!(run.session_start()["credential_source"], "config");
}

/// S2: a `401` after the config changed is answered by one resend of the same bytes with the new
/// key, and the session never sees the rejection.
#[test]
fn rejected_request_rereads_and_retries_once() {
    let upstream = Upstream::start(&[0], |_, request| {
        if request.key() == NEW {
            (200, END_TURN)
        } else {
            (401, UNAUTHORIZED)
        }
    });
    let capsule = Capsule::new(&upstream, ApiKey::Reference, "");
    capsule.set_credential(OLD);
    let child = capsule.spawn_run(None);
    upstream.wait_for(1);
    capsule.set_credential(NEW);
    upstream.release(0);
    let run = Run::of(child.wait_with_output().unwrap());

    assert!(run.ok(), "{}", run.context());
    let requests = upstream.requests();
    let keys: Vec<_> = requests.iter().map(|r| label(r.key())).collect();
    println!("upstream x-api-key sequence: {keys:?}");
    assert_eq!(keys, ["old", "new"]);
    assert_eq!(requests[0].body, requests[1].body, "byte-identical bodies");
    let result = run.result();
    assert!(!result.contains("error"), "{result}");

    let events = run.credential_events();
    println!("inference_credential events: {events:?}");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["change"], "rotated");
    assert_eq!(events[0]["trigger"], "rejection");
    assert!(events.iter().all(|event| event["change"] != "rejected"));
}

/// S3: a rejection that re-reading does not cure ends the task as `E-RUN-026`, naming where the
/// key came from, in place of the driver's own error.
#[test]
fn persistent_rejection_names_the_credential_source() {
    enum Case {
        ConfigUnchanged,
        ConfigChangedAndRejected,
        Environment,
    }
    for case in [
        Case::ConfigUnchanged,
        Case::ConfigChangedAndRejected,
        Case::Environment,
    ] {
        let hold: &[usize] = match case {
            Case::ConfigChangedAndRejected => &[0],
            _ => &[],
        };
        let upstream = Upstream::start(hold, |_, _| (401, UNAUTHORIZED));
        let capsule = Capsule::new(&upstream, ApiKey::Reference, "");
        let (run, expected_requests, retried) = match case {
            Case::ConfigUnchanged => {
                capsule.set_credential(OLD);
                (capsule.run(None), 1, false)
            }
            Case::ConfigChangedAndRejected => {
                capsule.set_credential(OLD);
                let child = capsule.spawn_run(None);
                upstream.wait_for(1);
                capsule.set_credential(NEW2);
                upstream.release(0);
                (Run::of(child.wait_with_output().unwrap()), 2, true)
            }
            Case::Environment => (capsule.run(Some(OLD)), 1, false),
        };

        let requests = upstream.requests();
        let keys: Vec<_> = requests.iter().map(|r| label(r.key())).collect();
        println!("upstream x-api-key sequence: {keys:?}");
        assert_eq!(requests.len(), expected_requests, "{}", run.context());
        // `status:` reports the launch; the task's own outcome is the trace's `task_end`.
        let task_end = run
            .trace()
            .into_iter()
            .find(|event| event["event_type"] == "task_end")
            .expect("task_end");
        assert_eq!(task_end["exit_status"], "failed", "{}", run.context());

        let error_lines = run.lines("E-RUN-026");
        println!("{}", error_lines.join("\n"));
        assert_eq!(error_lines.len(), 1, "{}", run.context());
        let line = error_lines[0];
        match case {
            Case::Environment => {
                assert!(
                    line.contains(&format!("environment variable {NAME}")),
                    "{line}"
                );
                assert!(line.contains("read once at launch"), "{line}");
            }
            _ => {
                assert!(line.contains(&format!("credentials.{NAME}")), "{line}");
                assert!(
                    line.contains(&capsule.config_path().display().to_string()),
                    "{line}"
                );
            }
        }

        let result = run.result();
        assert!(
            result.contains("the provider rejected the inference credential"),
            "{result}"
        );
        assert!(!result.contains("inference error from driver"), "{result}");
        assert!(!run.stderr.contains("inference error from driver"));

        let events = run.credential_events();
        println!("inference_credential events: {events:?}");
        let rejected: Vec<_> = events
            .iter()
            .filter(|event| event["change"] == "rejected")
            .collect();
        assert_eq!(rejected.len(), 1, "{events:?}");
        assert_eq!(rejected[0]["status"], 401);
        assert_eq!(rejected[0]["retried"], retried);
    }
}

/// S4: a key only readable at launch warns once on every surface that describes a launch, and a
/// key from the config neither warns nor loses to the environment.
#[test]
fn launch_only_credential_warns() {
    let end_turn = || Upstream::start(&[], |_, _| (200, END_TURN));

    // (A) the environment supplies the key; (B) the manifest writes it literally.
    for (api_key, env_key, names) in [
        (ApiKey::Reference, Some(OLD), NAME),
        (ApiKey::Literal, None, "written literally"),
    ] {
        let upstream = end_turn();
        let capsule = Capsule::new(&upstream, api_key, "");
        let run = capsule.run(env_key);
        assert!(run.ok(), "{}", run.context());
        for (surface, output) in [
            ("run", run),
            ("run --explain-scope", capsule.explain_scope(env_key)),
            ("doctor", capsule.doctor(env_key)),
        ] {
            let lines = output.lines("W-SEC-026");
            println!("{surface}: {}", lines.join("\n"));
            assert_eq!(lines.len(), 1, "{surface}: {}", output.context());
            assert!(lines[0].contains(W_SEC_026_LINK), "{}", lines[0]);
            assert!(lines[0].contains(names), "{}", lines[0]);
            assert!(
                lines[0].contains("cannot pick up a rotated key until it is restarted"),
                "{}",
                lines[0]
            );
            for key in [OLD, LITERAL] {
                assert!(!output.stderr.contains(key), "{surface}");
                assert!(!output.stdout.contains(key), "{surface}");
            }
        }
    }

    // (C) the config holds the key: nothing warns, and it wins over the environment.
    let upstream = end_turn();
    let capsule = Capsule::new(&upstream, ApiKey::Reference, "");
    capsule.set_credential(OLD);
    let run = capsule.run(Some(NEW));
    assert!(run.ok(), "{}", run.context());
    assert_eq!(run.session_start()["credential_source"], "config");
    let requests = upstream.requests();
    println!("upstream x-api-key: {}", label(requests[0].key()));
    assert_eq!(requests[0].key(), OLD);
    for (surface, output) in [
        ("run", run),
        ("run --explain-scope", capsule.explain_scope(Some(NEW))),
        ("doctor", capsule.doctor(Some(NEW))),
    ] {
        assert!(
            output.lines("W-SEC-026").is_empty(),
            "{surface}: {}",
            output.context()
        );
    }
}

/// S9 through the binary: `mur config set -g credentials.NAME` confirms without the value, and
/// setting the `inference.api_key` scalar prints the note pointing at `credentials.<NAME>`.
#[test]
fn config_set_confirms_the_credential_without_its_value() {
    let home = TempDir::new().unwrap();
    let mur = |args: &[&str]| {
        let output = Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .env("HOME", home.path())
            .current_dir(home.path())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (stdout, stderr) = mur(&["config", "set", "-g", &format!("credentials.{NAME}"), OLD]);
    assert_eq!(
        stdout.trim(),
        format!("Set credentials.{NAME} in ~/.murmur/config.yaml")
    );
    assert!(
        !stdout.contains(OLD) && !stderr.contains(OLD),
        "{stdout}{stderr}"
    );

    let (_, stderr) = mur(&["config", "set", "-g", "inference.api_key", LITERAL]);
    assert!(
        stderr.contains("note: mur run does not read inference.api_key"),
        "{stderr}"
    );
    assert!(stderr.contains("credentials.<NAME>"), "{stderr}");
}

/// S5: a credential held nowhere refuses the launch before anything is sent or created.
#[test]
fn missing_credential_refuses_before_launch() {
    let upstream = Upstream::start(&[], |_, _| (200, END_TURN));
    let capsule = Capsule::new(&upstream, ApiKey::Reference, "");
    let run = capsule.run(None);
    let combined = format!("{}{}", run.stdout, run.stderr);
    println!("{}", combined.trim());
    assert!(!run.output.status.success(), "{combined}");
    assert!(combined.contains("E-MAN-003"), "{combined}");
    assert!(
        combined.contains(&format!("credentials.{NAME}")),
        "{combined}"
    );
    assert!(
        combined.contains(&capsule.config_path().display().to_string()),
        "{combined}"
    );
    assert!(
        combined.contains(&format!("environment variable {NAME}")),
        "{combined}"
    );
    assert!(upstream.requests().is_empty());
    let sessions: Vec<_> = [capsule.project.path(), capsule.home.path()]
        .into_iter()
        .flat_map(|root| directories_named_like(root, "ses_"))
        .collect();
    assert!(sessions.is_empty(), "{sessions:?}");
}

/// S6: through a rotation, at every `trace.capture` setting, neither key is written anywhere the
/// session or `mur config set` writes, other than the config file itself.
#[test]
fn rotated_key_never_recorded() {
    for capture in ["none", "meta", "content"] {
        let flow = rotation_flow(&format!("trace:\n  capture: {capture}\n"));
        assert!(flow.run.ok(), "{capture}: {}", flow.run.context());
        assert_eq!(flow.upstream.requests().len(), 2, "{capture}");
        let config = flow.capsule.config_path();

        for key in [OLD, NEW] {
            let mut leaks = files_containing(flow.capsule.project.path(), key.as_bytes(), &config);
            leaks.extend(files_containing(
                flow.capsule.home.path(),
                key.as_bytes(),
                &config,
            ));
            println!(
                "trace.capture={capture}: files containing the {} key: {leaks:?}",
                label(key)
            );
            assert!(leaks.is_empty(), "{capture}: {leaks:?}");
            assert!(!flow.run.stdout.contains(key), "{capture}: run stdout");
            assert!(!flow.run.stderr.contains(key), "{capture}: run stderr");
            for output in &flow.set_outputs {
                assert!(
                    !String::from_utf8_lossy(&output.stdout).contains(key),
                    "{capture}: config set stdout"
                );
                assert!(
                    !String::from_utf8_lossy(&output.stderr).contains(key),
                    "{capture}: config set stderr"
                );
            }
        }
    }
}
