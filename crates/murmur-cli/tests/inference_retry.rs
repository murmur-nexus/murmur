//! A `transport: http` driver call that fails transiently is made again: the same payload through
//! the same driver, after a capped, jittered backoff or the wait the provider asked for, within
//! `inference.retry`'s bounds. Each retry writes one `inference_retry` line, and the call keeps its
//! single `inference` record.
//!
//! Driven through the real `mur` binary against a loopback upstream the test controls: it records
//! each request's body and arrival, and answers each with a status, headers and body of its
//! choosing, or by closing the connection without a word.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const RETRY_DRIVER: &str = "local/retry-driver";
const VERSION: &str = "0.1.0";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const HOOK: &str = "retrying-hook";
/// What every capsule here sets unless its case says otherwise.
const FAST_RETRY: &str = "  retry:\n    initial_delay_ms: 10\n    max_delay_ms: 2000\n";
const WAIT: Duration = Duration::from_secs(240);

const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
/// The host's own driver response shape, which the retry driver passes through.
const HOST_END_TURN: &str =
    r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}"#;
const RATE_LIMITED: &str =
    r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
const OVERLOADED: &str =
    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

#[derive(Clone)]
struct Recorded {
    body: Vec<u8>,
    arrived: Instant,
}

/// How the upstream answers one request.
enum Reply {
    /// A response with this status, these extra headers and this body.
    Answer {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
    /// The connection closed after the request was read, with nothing written.
    Drop,
    /// A `200` head promising more body than is written, this much of it, then the connection
    /// closed: a stream that broke after the head.
    Partial(String),
}

fn answer(status: u16, body: &str) -> Reply {
    Reply::Answer {
        status,
        headers: Vec::new(),
        body: body.to_string(),
    }
}

fn answer_with(status: u16, headers: &[(&'static str, &str)], body: &str) -> Reply {
    Reply::Answer {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (*name, value.to_string()))
            .collect(),
        body: body.to_string(),
    }
}

type Respond = dyn Fn(usize) -> Reply + Send + Sync;

struct Upstream {
    endpoint: String,
    shared: Arc<(Mutex<Vec<Recorded>>, Condvar)>,
}

impl Upstream {
    /// Answers request `n` (zero-based) with `respond(n)`.
    fn start(respond: impl Fn(usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let shared = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let respond: Arc<Respond> = Arc::new(respond);
        let thread_shared = Arc::clone(&shared);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some(request) = common::recording_upstream::read_request(&mut stream) else {
                    continue;
                };
                let index = {
                    let (lock, condvar) = &*thread_shared;
                    let mut requests = lock.lock().unwrap();
                    requests.push(Recorded {
                        body: request.body,
                        arrived: request.arrived,
                    });
                    condvar.notify_all();
                    requests.len() - 1
                };
                match respond(index) {
                    Reply::Drop => drop(stream),
                    Reply::Partial(body) => write_partial(&mut stream, &body),
                    Reply::Answer {
                        status,
                        headers,
                        body,
                    } => write_response(&mut stream, status, &headers, &body),
                }
            }
        });
        Self { endpoint, shared }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.shared.0.lock().unwrap().clone()
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&'static str, String)],
    body: &str,
) {
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\n{extra}\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn write_partial(stream: &mut TcpStream, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len() + 4096
    );
    let _ = stream.flush();
}

/// A port nothing listens on.
fn refused_endpoint() -> String {
    format!("http://127.0.0.1:{}", common::free_port())
}

/// A scratch `HOME` with both drivers and the retrying hook published, and each driver
/// precompiled so a run does not spend its time compiling.
struct Home {
    home: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        for (name, wasm) in [
            (
                ANTHROPIC,
                "drivers/anthropic/driver/murmur-driver-anthropic.wasm",
            ),
            (RETRY_DRIVER, "retry-driver/tool/retry-driver.wasm"),
        ] {
            let driver = common::create_driver_artifact_with_auth(
                artifacts.path(),
                name,
                VERSION,
                &common::fixture_path(wasm),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            );
            common::publish_local(&home, &driver).success();
            Command::new(assert_cmd::cargo::cargo_bin("mur"))
                .env("HOME", home.path())
                .arg("precompile")
                .arg(&driver)
                .output()
                .unwrap();
        }
        let hook = common::hook_wat::create_hook_zip(
            artifacts.path(),
            HOOK,
            "on-inference",
            "none",
            &common::hook_wat::run_inference_then_artifact_hook_wasm(),
        );
        common::publish_local(&home, &hook).success();
        Self { home }
    }

    fn mur(&self, project: &Path) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command.env("HOME", self.home.path()).current_dir(project);
        command
    }

    /// A project whose capsule runs `driver` against `endpoint`, with `artifacts` appended to its
    /// `artifacts:` list and `inference` to its `inference:` block.
    fn capsule(&self, driver: &str, endpoint: &str, artifacts: &str, inference: &str) -> Capsule {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: retry-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: sk-retry-test-key\n{artifacts}inference:\n  \
                 transport: http\n  model: test-model\n  driver:\n    artifact: {driver}\n\
                 {inference}"
            ),
        )
        .unwrap();
        Capsule { project, manifest }
    }

    fn spawn(&self, capsule: &Capsule, extra: &[&str]) -> std::process::Child {
        self.mur(capsule.project.path())
            .args(["run", "--manifest", capsule.manifest.to_str().unwrap()])
            .args(extra)
            .args(["--task", "Say hello.", "--verbose"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, capsule: &Capsule) -> Run {
        self.run_with(capsule, &[])
    }

    fn run_with(&self, capsule: &Capsule, extra: &[&str]) -> Run {
        let started = Instant::now();
        let mut child = self.spawn(capsule, extra);
        // Bounded: a run that hangs fails the test rather than the suite.
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if started.elapsed() > WAIT {
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Run::of(child.wait_with_output().unwrap(), started.elapsed())
    }

    /// The first `trace.jsonl` under this home's records holding an `event_type` line, waited for.
    fn wait_for_trace_line(&self, event_type: &str) -> PathBuf {
        let needle = format!("\"event_type\":\"{event_type}\"");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(trace) = find_files(self.home.path(), "trace.jsonl")
                .into_iter()
                .find(|path| fs::read_to_string(path).is_ok_and(|text| text.contains(&needle)))
            {
                return trace;
            }
            assert!(Instant::now() < deadline, "no {event_type} line appeared");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn find_files(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                found.push(path);
            }
        }
    }
    found
}

struct Capsule {
    project: TempDir,
    manifest: PathBuf,
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

impl Run {
    fn of(output: Output, elapsed: Duration) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
            elapsed,
        }
    }

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout)
    }

    fn result(&self) -> String {
        fs::read_to_string(self.records_dir().join("out/result.txt"))
            .unwrap_or_else(|err| panic!("no out/result.txt ({err}); {}", self.context()))
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.records_dir().join("trace.jsonl"))
            .unwrap_or_else(|err| panic!("no trace.jsonl ({err}); {}", self.context()))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn retries(&self) -> Vec<Value> {
        self.of_type("inference_retry")
    }

    /// The agent loop's own `inference` records: every one without an `origin`.
    fn turn_records(&self) -> Vec<Value> {
        self.of_type("inference")
            .into_iter()
            .filter(|event| event.get("origin").is_none())
            .collect()
    }

    /// The single agent-loop `inference` record, asserted to be the only one.
    fn the_turn_record(&self) -> Value {
        let records = self.turn_records();
        assert_eq!(records.len(), 1, "{records:#?}\n{}", self.context());
        records[0].clone()
    }

    fn exit_status(&self) -> Value {
        self.of_type("session_end")[0]["exit_status"].clone()
    }

    fn task_failed(&self) -> Value {
        let failed = self.of_type("task_failed");
        assert_eq!(failed.len(), 1, "{}", self.context());
        failed[0].clone()
    }

    fn task_failed_cause(&self) -> Value {
        self.task_failed()["cause"].clone()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

/// `inference.retry`'s fast settings with `max_retries` set.
fn retry_block(max_retries: u32, initial_delay_ms: u32, max_delay_ms: u32) -> String {
    format!(
        "  retry:\n    max_retries: {max_retries}\n    initial_delay_ms: \
         {initial_delay_ms}\n    max_delay_ms: {max_delay_ms}\n"
    )
}

fn assert_no_retry_fields(record: &Value) {
    assert!(record.get("attempts").is_none(), "{record}");
    assert!(record.get("retry_stopped").is_none(), "{record}");
}

/// A `429` and then an answer completes the task. The retry is one line, the call one record
/// carrying `attempts: 2`, and both requests carry the same bytes.
#[test]
fn a_rate_limit_is_retried_and_the_task_completes() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => answer(429, RATE_LIMITED),
        _ => answer(200, END_TURN),
    });
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(run.result(), "done");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.context());
    assert_eq!(
        requests[0].body, requests[1].body,
        "every attempt sends the same bytes"
    );

    let retries = run.retries();
    assert_eq!(retries.len(), 1, "{}", run.context());
    let retry = &retries[0];
    println!("{retry}");
    assert_eq!(retry["attempt"], 1);
    assert_eq!(retry["reason"], "status");
    assert_eq!(retry["provider_status"], 429);
    assert_eq!(retry["charged_tokens"], 0);
    assert_eq!(retry["max_retries"], 6);
    assert_eq!(retry["turn"], 0);
    let delay = retry["delay_ms"].as_u64().unwrap();
    assert!((5..=10).contains(&delay), "{retry}");
    assert!(retry.get("retry_after_ms").is_none(), "{retry}");
    assert!(retry.get("origin").is_none(), "{retry}");

    let record = run.the_turn_record();
    assert_eq!(record["decision"], "end_turn", "{record}");
    assert_eq!(record["attempts"], 2, "{record}");
    assert!(record.get("retry_stopped").is_none(), "{record}");
    assert_eq!(run.exit_status(), "ok");

    let show = home
        .mur(capsule.project.path())
        .args(["trace", "show"])
        .output()
        .unwrap();
    let show = String::from_utf8_lossy(&show.stdout);
    println!("{show}");
    assert!(
        show.contains("inference_retry  attempt 1 of 7  HTTP 429  wait"),
        "{show}"
    );
}

/// Each of several server errors in a row is retried, the backoff doubling between them.
#[test]
fn consecutive_server_errors_are_each_retried() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => answer(503, OVERLOADED),
        1 => answer(529, OVERLOADED),
        _ => answer(200, END_TURN),
    });
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(upstream.requests().len(), 3);
    let retries = run.retries();
    assert_eq!(retries.len(), 2, "{}", run.context());
    assert_eq!(retries[0]["attempt"], 1);
    assert_eq!(retries[0]["provider_status"], 503);
    assert_eq!(retries[1]["attempt"], 2);
    assert_eq!(retries[1]["provider_status"], 529);
    let second = retries[1]["delay_ms"].as_u64().unwrap();
    assert!((10..=20).contains(&second), "{}", retries[1]);
    assert_eq!(run.the_turn_record()["attempts"], 3);
}

/// A `retry-after` replaces the backoff, exactly.
#[test]
fn retry_after_is_honoured() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => answer_with(429, &[("retry-after", "2")], RATE_LIMITED),
        _ => answer(200, END_TURN),
    });
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2);
    let gap = requests[1].arrived - requests[0].arrived;
    assert!(gap >= Duration::from_millis(2000), "{gap:?}");
    let retry = &run.retries()[0];
    assert_eq!(retry["retry_after_ms"], 2000, "{retry}");
    assert_eq!(retry["delay_ms"], 2000, "{retry}");
}

/// A call that keeps failing stops after `max_retries` and fails the task, naming why; with
/// `max_retries: 0` it is not retried at all.
#[test]
fn exhausted_retries_fail_the_task_naming_why() {
    let home = Home::new();
    let upstream = Upstream::start(|_| answer(503, OVERLOADED));
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", &retry_block(2, 10, 2000));
    let run = home.run(&capsule);

    assert_eq!(upstream.requests().len(), 3, "{}", run.context());
    assert_eq!(run.retries().len(), 2);
    let record = run.the_turn_record();
    println!("{record}");
    assert_eq!(record["error_code"], "driver_error", "{record}");
    assert_eq!(record["provider_status"], 503, "{record}");
    assert_eq!(record["attempts"], 3, "{record}");
    assert_eq!(record["retry_stopped"], "max_retries", "{record}");
    assert_eq!(run.task_failed_cause(), "driver_error");
    assert_eq!(run.exit_status(), "failed");
    assert_ne!(run.output.status.code(), Some(0));
    let reason = run.task_failed()["reason"].as_str().unwrap().to_string();
    println!("{reason}");
    assert!(
        reason.ends_with(" (3 attempts; retry stopped: max_retries)"),
        "{reason}"
    );
    assert!(reason.starts_with("HTTP 503"), "{reason}");
    assert!(run.stderr.contains("3 attempts"), "{}", run.context());
    assert_eq!(
        record["error"]
            .as_str()
            .map(|error| error.contains("attempt")),
        Some(false),
        "the record's error is the provider's alone: {record}"
    );

    let once = Upstream::start(|_| answer(503, OVERLOADED));
    let capsule = home.capsule(ANTHROPIC, &once.endpoint, "", &retry_block(0, 10, 2000));
    let run = home.run(&capsule);
    assert_eq!(once.requests().len(), 1, "{}", run.context());
    assert!(run.retries().is_empty());
    let record = run.the_turn_record();
    assert_eq!(record["retry_stopped"], "max_retries", "{record}");
    assert!(record.get("attempts").is_none(), "{record}");
    let reason = run.task_failed()["reason"].as_str().unwrap().to_string();
    assert!(
        reason.ends_with(" (1 attempt; retry stopped: max_retries)"),
        "{reason}"
    );
    assert!(
        run.stderr.contains("1 attempt; retry stopped"),
        "{}",
        run.context()
    );
}

/// A failure that is not transient is not retried, and fails the task with its ordinary cause.
#[test]
fn non_transient_failures_are_not_retried() {
    let home = Home::new();
    let cases: [(u16, &str, &str); 4] = [
        (
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
            "driver_error",
        ),
        (
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Output blocked by content filtering policy"}}"#,
            "driver_error",
        ),
        (
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: test-model"}}"#,
            "driver_error",
        ),
        (
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            "credential_rejected",
        ),
    ];
    let runs: Vec<_> = cases
        .into_iter()
        .map(|(status, body, cause)| {
            let upstream = Upstream::start(move |_| answer(status, body));
            let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", FAST_RETRY);
            (status, cause, upstream, capsule)
        })
        .collect();
    for (status, cause, upstream, capsule) in &runs {
        let run = home.run(capsule);
        println!("{status}: {}", run.the_turn_record());
        assert_eq!(upstream.requests().len(), 1, "{status}: {}", run.context());
        assert!(run.retries().is_empty(), "{status}");
        let record = run.the_turn_record();
        assert_no_retry_fields(&record);
        assert_eq!(record["provider_status"], *status, "{record}");
        assert_eq!(run.task_failed_cause(), *cause, "{status}");
        let reason = run.task_failed()["reason"].to_string();
        assert!(!reason.contains("attempt"), "{status}: {reason}");
    }
}

/// A connection that closes before a response is retried; a refused connection is not.
#[test]
fn a_dropped_connection_is_retried_and_a_refused_one_is_not() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => Reply::Drop,
        _ => answer(200, END_TURN),
    });
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(upstream.requests().len(), 2);
    let retries = run.retries();
    assert_eq!(retries.len(), 1, "{}", run.context());
    println!("{}", retries[0]);
    assert_eq!(retries[0]["reason"], "connection");
    assert!(retries[0].get("provider_status").is_none());
    assert_eq!(retries[0]["charged_tokens"], 0);

    let capsule = home.capsule(ANTHROPIC, &refused_endpoint(), "", FAST_RETRY);
    let run = home.run(&capsule);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.retries().is_empty(), "{}", run.context());
    let record = run.the_turn_record();
    assert!(record.get("retry_stopped").is_none(), "{record}");
}

/// A failure the driver marks transient is retried and charged as a failed call; one it marks
/// not transient, or does not mark, is not retried.
#[test]
fn a_driver_marked_transient_failure_is_retried() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => answer(
            200,
            r#"{"fixture_break":{"error":"stream broke","transient":"true"}}"#,
        ),
        _ => answer(200, HOST_END_TURN),
    });
    let capsule = home.capsule(RETRY_DRIVER, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(run.result(), "done");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    let retries = run.retries();
    assert_eq!(retries.len(), 1, "{}", run.context());
    println!("{}", retries[0]);
    assert_eq!(retries[0]["reason"], "driver");
    assert!(retries[0].get("provider_status").is_none());
    assert_eq!(retries[0]["error"], "stream broke");
    let record = run.the_turn_record();
    assert_eq!(record["attempts"], 2, "{record}");
    assert_eq!(retries[0]["charged_tokens"], record["input_tokens"]);

    for transient in [r#","transient":"false""#, ""] {
        let body = format!(r#"{{"fixture_break":{{"error":"stream broke"{transient}}}}}"#);
        let upstream = Upstream::start(move |_| answer(200, &body));
        let capsule = home.capsule(RETRY_DRIVER, &upstream.endpoint, "", FAST_RETRY);
        let run = home.run(&capsule);
        assert_eq!(
            upstream.requests().len(),
            1,
            "{transient}: {}",
            run.context()
        );
        assert_ne!(run.output.status.code(), Some(0));
        assert!(run.retries().is_empty(), "{transient}");
        assert_no_retry_fields(&run.the_turn_record());
    }
}

/// A response that breaks off mid-body after its `200` head is retried when the driver marks the
/// break transient, and the turn commits only the retry's answer.
#[test]
fn a_stream_dropped_mid_body_is_retried_and_only_the_retry_is_committed() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        0 => Reply::Partial(
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"partial-first-attempt"#
                .to_string(),
        ),
        _ => answer(200, HOST_END_TURN),
    });
    let capsule = home.capsule(RETRY_DRIVER, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.context());
    assert_eq!(requests[0].body, requests[1].body);
    let retries = run.retries();
    assert_eq!(retries.len(), 1, "{}", run.context());
    println!("{}", retries[0]);
    assert_eq!(retries[0]["reason"], "driver");
    assert!(
        retries[0].get("provider_status").is_none(),
        "{}",
        retries[0]
    );
    assert_eq!(run.the_turn_record()["attempts"], 2);
    let result = run.result();
    assert_eq!(result, "done");
    assert!(!result.contains("partial-first-attempt"));
}

/// A driver's `transient: "false"` vetoes a retry of a status that would otherwise be retried.
#[test]
fn a_driver_can_veto_a_retryable_status() {
    let home = Home::new();
    let upstream = Upstream::start(|_| answer(429, r#"{"fixture_transient":"false"}"#));
    let capsule = home.capsule(RETRY_DRIVER, &upstream.endpoint, "", FAST_RETRY);
    let run = home.run(&capsule);

    assert_eq!(upstream.requests().len(), 1, "{}", run.context());
    assert!(run.retries().is_empty());
    let record = run.the_turn_record();
    assert_eq!(record["provider_status"], 429, "{record}");
    assert_no_retry_fields(&record);
}

/// A wait that would end past the session deadline is not taken; the call fails at once.
#[test]
fn a_wait_never_runs_past_the_deadline() {
    let home = Home::new();
    let upstream = Upstream::start(|_| answer_with(429, &[("retry-after", "1200")], RATE_LIMITED));
    let capsule = home.capsule(
        ANTHROPIC,
        &upstream.endpoint,
        "",
        &retry_block(6, 10, 1_800_000),
    );
    let run = home.run_with(&capsule, &["--deadline", "300"]);

    assert!(run.elapsed < Duration::from_secs(120), "{:?}", run.elapsed);
    assert_eq!(upstream.requests().len(), 1, "{}", run.context());
    assert!(run.retries().is_empty());
    let record = run.the_turn_record();
    assert_eq!(record["retry_stopped"], "deadline", "{record}");
    assert_eq!(run.task_failed_cause(), "driver_error");
    assert!(run.of_type("deadline_reached").is_empty());
}

/// A provider asking for a wait longer than `max_delay_ms` stops the retrying.
#[test]
fn a_retry_after_beyond_the_cap_stops_retrying() {
    let home = Home::new();
    let upstream = Upstream::start(|_| answer_with(429, &[("retry-after", "120")], RATE_LIMITED));
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", &retry_block(6, 10, 5000));
    let run = home.run(&capsule);

    assert!(run.elapsed < Duration::from_secs(60), "{:?}", run.elapsed);
    assert_eq!(upstream.requests().len(), 1, "{}", run.context());
    assert!(run.retries().is_empty());
    assert_eq!(run.the_turn_record()["retry_stopped"], "retry_after");
}

/// Every attempt is admitted against the spend ceiling. A charged failed attempt can leave
/// too little for the next, which then ends the session as any refused call does.
#[test]
fn every_attempt_is_admitted_by_the_spend_ceiling() {
    let home = Home::new();
    let upstream = Upstream::start(|_| {
        answer(
            200,
            r#"{"fixture_break":{"error":"stream broke","transient":"true","usage":{"input_tokens":99000,"output_tokens":0}}}"#,
        )
    });
    let capsule = home.capsule(
        RETRY_DRIVER,
        &upstream.endpoint,
        "",
        &format!("  max_session_tokens: 100000\n  max_tokens: 1000\n{FAST_RETRY}"),
    );
    let run = home.run(&capsule);

    assert_eq!(upstream.requests().len(), 1, "{}", run.context());
    let trace = run.trace();
    let retry_at = trace
        .iter()
        .position(|event| event["event_type"] == "inference_retry")
        .unwrap_or_else(|| panic!("no inference_retry; {}", run.context()));
    assert_eq!(trace[retry_at]["charged_tokens"], 99000);
    let refusal_at = trace
        .iter()
        .position(|event| event["event_type"] == "spend_ceiling_reached")
        .expect("spend_ceiling_reached");
    assert!(retry_at < refusal_at);
    assert_eq!(trace[refusal_at]["limit"], "session");
    assert_eq!(run.exit_status(), "spend_ceiling_reached");
}

/// A cancel during the wait ends the task as canceled, in phase `inference`.
#[test]
fn a_cancel_during_the_wait_ends_the_task() {
    let home = Home::new();
    let upstream = Upstream::start(|_| answer_with(429, &[("retry-after", "60")], RATE_LIMITED));
    let capsule = home.capsule(
        ANTHROPIC,
        &upstream.endpoint,
        "",
        &retry_block(6, 10, 120_000),
    );
    let child = home.spawn(&capsule, &[]);
    home.wait_for_trace_line("inference_retry");
    let stopped_at = Instant::now();
    let stop = home
        .mur(capsule.project.path())
        .args(["stop", "@1"])
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    let run = Run::of(child.wait_with_output().unwrap(), stopped_at.elapsed());

    assert!(run.elapsed < Duration::from_secs(20), "{:?}", run.elapsed);
    assert_eq!(upstream.requests().len(), 1, "{}", run.context());
    let canceled = run.of_type("task_canceled");
    assert_eq!(canceled.len(), 1, "{}", run.context());
    assert_eq!(canceled[0]["phase"], "inference", "{}", canceled[0]);
}

/// A hook's `run-inference` is retried like an agent-loop call, its retry tagged with the
/// hook's origin and its record carrying the attempts.
#[test]
fn a_hook_run_inference_is_retried() {
    let home = Home::new();
    let upstream = Upstream::start(|n| match n {
        1 => answer(429, RATE_LIMITED),
        _ => answer(200, END_TURN),
    });
    let capsule = home.capsule(
        ANTHROPIC,
        &upstream.endpoint,
        &format!("  - name: local/{HOOK}\n    version: 0.1.0\n    runtime: hook\n"),
        FAST_RETRY,
    );
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert_eq!(upstream.requests().len(), 3, "{}", run.context());
    let retries = run.retries();
    assert_eq!(retries.len(), 1, "{}", run.context());
    println!("{}", retries[0]);
    assert_eq!(retries[0]["origin"], format!("hook:{HOOK}"));
    assert_eq!(retries[0]["provider_status"], 429);
    assert_eq!(retries[0]["charged_tokens"], 0);
    let hook_records: Vec<Value> = run
        .of_type("inference")
        .into_iter()
        .filter(|event| event["origin"] == format!("hook:{HOOK}"))
        .collect();
    assert_eq!(hook_records.len(), 1, "{hook_records:#?}");
    assert_eq!(
        hook_records[0]["decision"], "end_turn",
        "{}",
        hook_records[0]
    );
    assert_eq!(hook_records[0]["attempts"], 2, "{}", hook_records[0]);
    assert_no_retry_fields(&run.the_turn_record());
}
