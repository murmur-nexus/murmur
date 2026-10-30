//! `capabilities.limits.deadline_seconds` stops a component call that waits on the host: a driver
//! that never sends its request, a provider that never answers, a tool that never comes back and
//! a hook that sleeps through its deadline are each stopped at the deadline plus the grace, with
//! one `call_deadline` line naming what was stopped. Every `inference` record of a call made
//! through the runtime's credential gateway says when the request reached the gateway and when
//! the provider answered, so a driver that never sent reads differently from a slow provider.
//!
//! Driven through the real `mur` binary against loopback upstreams the test owns.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const STALL_DRIVER: &str = "local/stall-driver";
const STALL_TOOL: &str = "local/stall-tool";
const SLEEP_HOOK: &str = "local/sleep-hook";
const VERSION: &str = "0.1.0";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
/// Far above every deadline here plus its grace: a run that reaches it was never stopped.
const WAIT: Duration = Duration::from_secs(120);
const STOPPED: &str = "waited on the host past its 3s execution deadline";

const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

/// A capsule-wide `deadline_seconds: 3`.
const DEADLINE_3: &str = "capabilities:\n  limits:\n    deadline_seconds: 3\n";

/// A scratch `HOME` with both drivers, the stalling tool and the sleeping hook published, and the
/// components precompiled so a run spends its deadline on the call, not on compiling.
struct Home {
    home: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let mut published = Vec::new();
        for (name, wasm) in [
            (
                ANTHROPIC,
                "drivers/anthropic/driver/murmur-driver-anthropic.wasm",
            ),
            (STALL_DRIVER, "stall-driver/tool/stall-driver.wasm"),
        ] {
            published.push(common::create_driver_artifact_with_auth(
                artifacts.path(),
                name,
                VERSION,
                &common::fixture_path(wasm),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            ));
        }
        published.push(common::create_tool_artifact(
            artifacts.path(),
            STALL_TOOL,
            VERSION,
            &common::fixture_path("stall-driver/tool/stall-driver.wasm"),
        ));
        for artifact in &published {
            common::publish_local(&home, artifact).success();
            Command::new(assert_cmd::cargo::cargo_bin("mur"))
                .env("HOME", home.path())
                .arg("precompile")
                .arg(artifact)
                .output()
                .unwrap();
        }
        common::publish_local(&home, &sleep_hook_zip(artifacts.path())).success();
        Self { home }
    }

    /// A project whose capsule runs `driver` behind `gateway:` at `endpoint`, with `artifacts`
    /// appended to its `artifacts:` list and `capabilities` (a whole top-level block, or empty)
    /// after it.
    fn capsule(
        &self,
        driver: &str,
        endpoint: &str,
        artifacts: &str,
        capabilities: &str,
    ) -> Capsule {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: deadline-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: sk-deadline-test-key\n{artifacts}{capabilities}\
                 inference:\n  transport: http\n  model: test-model\n  max_turns: 4\n  driver:\n    \
                 artifact: {driver}\n"
            ),
        )
        .unwrap();
        Capsule { project, manifest }
    }

    fn run(&self, capsule: &Capsule) -> Run {
        let started = Instant::now();
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .env("HOME", self.home.path())
            .current_dir(capsule.project.path())
            .args(["run", "--manifest", capsule.manifest.to_str().unwrap()])
            .args(["--task", "Say hello.", "--verbose"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Bounded: a run that is never stopped fails the test rather than hanging the suite.
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > WAIT {
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let run = Run::of(child.wait_with_output().unwrap());
        assert!(
            started.elapsed() < WAIT,
            "the run was never stopped; {}",
            run.context()
        );
        run
    }
}

/// The sleeping hook, packed with its bundled manifest: `execution_mode: blocking` and no
/// `binding:`, so every session event reaches it.
fn sleep_hook_zip(dir: &Path) -> PathBuf {
    let path = dir.join(format!("sleep-hook-{VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("sleep-hook/murmur.yaml")).unwrap())
        .unwrap();
    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("sleep-hook/hook/sleep-hook.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    path
}

struct Capsule {
    project: TempDir,
    manifest: PathBuf,
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

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout)
    }

    fn trace(&self) -> Vec<Value> {
        let trace = self.records_dir().join("trace.jsonl");
        assert!(trace.is_file(), "no trace.jsonl; {}", self.context());
        common::read_whole_trace(&trace)
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// The position of the first line of `event_type` in the trace.
    fn position(&self, event_type: &str) -> usize {
        self.trace()
            .iter()
            .position(|event| event["event_type"] == event_type)
            .unwrap_or_else(|| panic!("no {event_type} line; {}", self.context()))
    }

    /// The one `call_deadline` line, asserted to be the only one.
    fn the_call_deadline(&self) -> Value {
        let cuts = self.of_type("call_deadline");
        assert_eq!(cuts.len(), 1, "{cuts:#?}\n{}", self.context());
        cuts[0].clone()
    }

    /// The single agent-loop `inference` record, asserted to be the only one.
    fn the_turn_record(&self) -> Value {
        let records: Vec<Value> = self
            .of_type("inference")
            .into_iter()
            .filter(|event| event.get("origin").is_none())
            .collect();
        assert_eq!(records.len(), 1, "{records:#?}\n{}", self.context());
        records[0].clone()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

/// Both gateway timing keys are written, with `null` values.
fn assert_null_key(record: &Value, key: &str) {
    let object = record.as_object().unwrap();
    assert!(object.contains_key(key), "{key} is written: {record}");
    assert_eq!(record[key], Value::Null, "{record}");
}

/// A driver that sleeps before sending anything is stopped at its deadline: one `call_deadline`
/// for the driver, then a failed `inference` that never reached the gateway, and the task fails
/// without a retry. The upstream never hears from it.
#[test]
fn a_driver_that_never_sends_is_stopped_and_reads_as_never_reaching_the_gateway() {
    let home = Home::new();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    upstream.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", upstream.local_addr().unwrap());
    let capsule = home.capsule(STALL_DRIVER, &endpoint, "", DEADLINE_3);
    let run = home.run(&capsule);

    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let cut = run.the_call_deadline();
    assert_eq!(cut["role"], "driver", "{cut}");
    assert_eq!(cut["component"], "stall-driver", "{cut}");
    assert_eq!(cut["call"], "run", "{cut}");
    assert_eq!(cut["deadline_seconds"], 3, "{cut}");
    assert!(cut["elapsed_ms"].as_u64().unwrap() >= 3_000, "{cut}");
    assert!(
        run.position("call_deadline") < run.position("inference"),
        "the call_deadline comes before the record of the call it cut"
    );

    let record = run.the_turn_record();
    assert_eq!(record["error_code"], "driver_failed", "{record}");
    assert!(
        record["error"].as_str().unwrap().contains(STOPPED),
        "{record}"
    );
    assert_null_key(&record, "gateway_reached_ms");
    assert_null_key(&record, "upstream_responded_ms");
    assert!(record.get("provider_status").is_none(), "{record}");
    let failed = run.of_type("task_failed");
    assert_eq!(failed.len(), 1, "{}", run.context());
    assert_eq!(failed[0]["cause"], "runtime_error");
    assert!(run.of_type("inference_retry").is_empty());
    assert!(
        upstream.accept().is_err(),
        "the stalled driver sent nothing upstream"
    );
}

/// A provider that never answers is stopped at the driver's deadline: the request reached the
/// gateway, no response head came back, and the call is not retried.
#[test]
fn a_provider_that_never_answers_reads_as_reached_with_no_response() {
    let home = Home::new();
    let capsule = home.capsule(ANTHROPIC, &common::never_replying(), "", DEADLINE_3);
    let run = home.run(&capsule);

    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let cut = run.the_call_deadline();
    assert_eq!(cut["role"], "driver", "{cut}");
    assert_eq!(cut["component"], "murmur-driver-anthropic", "{cut}");
    let record = run.the_turn_record();
    assert_eq!(record["error_code"], "driver_failed", "{record}");
    assert!(
        record["error"].as_str().unwrap().contains(STOPPED),
        "{record}"
    );
    let reached = record["gateway_reached_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("the request reached the gateway: {record}"));
    assert!(reached <= 5_000, "{record}");
    assert_null_key(&record, "upstream_responded_ms");
    assert!(record.get("provider_status").is_none(), "{record}");
    assert!(run.of_type("inference_retry").is_empty());
}

/// An answered call carries both offsets, the response after the reach, and nothing is stopped.
#[test]
fn an_answered_call_carries_both_gateway_offsets() {
    let home = Home::new();
    let upstream = common::ScriptedServer::start(vec![END_TURN.to_string()]);
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, "", DEADLINE_3);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let record = run.the_turn_record();
    let reached = record["gateway_reached_ms"].as_u64().expect("reached");
    let responded = record["upstream_responded_ms"].as_u64().expect("responded");
    assert!(reached <= responded, "{record}");
    assert!(run.of_type("call_deadline").is_empty(), "{}", run.context());
}

/// A tool call that never comes back is stopped at the deadline: its `call_deadline` comes before
/// its `tool_call`, the model reads the stop in the tool result, and the task goes on to finish.
#[test]
fn a_tool_that_never_returns_is_stopped_and_the_model_reads_why() {
    let home = Home::new();
    let upstream = common::ScriptedServer::start(vec![
        common::tool_use_response("toolu_stall", "stall-tool", json!({})),
        END_TURN.to_string(),
    ]);
    let tool = format!("  - name: {STALL_TOOL}\n    version: {VERSION}\n    runtime: tool\n");
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &tool, DEADLINE_3);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let cut = run.the_call_deadline();
    assert_eq!(cut["role"], "tool", "{cut}");
    assert_eq!(cut["component"], "stall-tool", "{cut}");
    assert_eq!(cut["call"], "run", "{cut}");
    assert!(
        run.position("call_deadline") < run.position("tool_call"),
        "the call_deadline comes before the tool_call it cut"
    );
    let calls = run.of_type("tool_call");
    assert_eq!(calls.len(), 1, "{}", run.context());
    assert!(
        calls[0]["error"]
            .as_str()
            .unwrap_or_default()
            .contains(STOPPED),
        "{}",
        calls[0]
    );
    let requests = upstream.requests();
    let result = common::find_tool_result(&requests, "toolu_stall")
        .map(|block| common::extract_result_text(&block))
        .unwrap_or_else(|| panic!("no tool result fed back: {requests:#?}"));
    assert!(result.contains(STOPPED), "{result}");
    let task_end = run.of_type("task_end");
    assert_eq!(task_end[0]["exit_status"], "ok", "{}", run.context());
}

/// A hook that sleeps through `on-session-start` is stopped at its entry's deadline and logged;
/// its instance is not entered again, and the session finishes its task.
#[test]
fn a_hook_that_sleeps_is_stopped_and_never_entered_again() {
    let home = Home::new();
    let upstream = common::ScriptedServer::start(vec![END_TURN.to_string()]);
    let hook = format!(
        "  - name: {SLEEP_HOOK}\n    version: {VERSION}\n    runtime: hook\n    capabilities:\n      \
         limits:\n        deadline_seconds: 2\n"
    );
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &hook, "");
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let cut = run.the_call_deadline();
    assert_eq!(cut["role"], "hook", "{cut}");
    assert_eq!(cut["component"], "sleep-hook", "{cut}");
    assert_eq!(cut["call"], "on-session-start", "{cut}");
    assert_eq!(cut["deadline_seconds"], 2, "{cut}");
    assert_eq!(run.of_type("task_end")[0]["exit_status"], "ok");

    let log = fs::read_to_string(run.records_dir().join("logs/hook-sleep-hook.log"))
        .unwrap_or_else(|err| panic!("no hook log ({err}); {}", run.context()));
    let lines: Vec<&str> = log.lines().collect();
    let stopped = lines
        .iter()
        .position(|line| {
            line.contains(
                "waited on the host past its 2s execution deadline \
                 (capabilities.limits.deadline_seconds on the artifacts: entry '",
            )
        })
        .unwrap_or_else(|| panic!("no stop logged:\n{log}"));
    assert!(
        lines[stopped + 1..]
            .iter()
            .any(|line| line.contains("was not called") && line.contains("on-session-start")),
        "a later call names the earlier stop:\n{log}"
    );
}
