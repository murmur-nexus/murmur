//! The runtime-provided `session-status` tool, the hard `mur run --deadline` and the one deadline
//! notice.
//!
//! Driven through the real `mur` binary: the http cases against the real anthropic driver fixture
//! and a scripted provider, the process cases against the fixture process driver and the fake
//! harness calling through the real tool bridge. Every number the tool returns is compared with
//! the trace the same session wrote, because agreeing with the trace is the property.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const PROCESS_DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Find out where this session stands.";
const NOTICE_PREFIX: &str = "Session deadline: the runtime ends this session at ";

// ── Scripted provider responses ───────────────────────────────────────────────

/// The provider's report on every scripted turn.
fn reported_usage() -> Value {
    json!({
        "input_tokens": 1200,
        "output_tokens": 30,
        "cache_read_input_tokens": 900,
        "cache_creation_input_tokens": 40,
    })
}

fn response(content: Value, stop_reason: &str, usage: Option<Value>) -> String {
    let mut body = json!({
        "id": "msg",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": stop_reason,
    });
    if let Some(usage) = usage {
        body["usage"] = usage;
    }
    body.to_string()
}

fn tool_turn(id: &str, name: &str, input: Value) -> String {
    response(
        json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        "tool_use",
        Some(reported_usage()),
    )
}

fn end_turn() -> String {
    response(
        json!([{"type": "text", "text": "done"}]),
        "end_turn",
        Some(reported_usage()),
    )
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the anthropic driver fixture published, its `models:` table `models`.
fn home_with_driver(models: &str) -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        &format!("{}{models}", common::FIXTURE_DRIVER_AUTH),
    );
    publish_compiled(&home, &artifact);
    home
}

/// Publish `artifact` into `home` and compile it there, so a launch's staging loads it rather
/// than compiling it: a deadline counts from `mur run`'s start, staging included.
fn publish_compiled(home: &TempDir, artifact: &Path) {
    common::publish_local(home, artifact).success();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .arg("precompile")
        .arg(artifact)
        .assert()
        .success();
}

/// What an http capsule declares beyond its driver.
#[derive(Default)]
struct Http<'a> {
    /// Grants `sleep` and keeps a 12-second sleep in the foreground.
    sleep: bool,
    inference: &'a str,
    lifecycle: &'a str,
    top: &'a str,
}

fn http_manifest(endpoint: &str, capsule: &Http<'_>) -> String {
    let shell = if capsule.sleep {
        "  shell:\n    allow:\n      - sleep\n"
    } else {
        ""
    };
    let grace = if capsule.sleep {
        "  shell_grace_secs: 60\n"
    } else {
        ""
    };
    format!(
        "name: status\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n{shell}\
         lifecycle:\n  task_acceptance: single\n{grace}{lifecycle}{top}inference:\n  transport: \
         http\n  model: test-model\n{inference}  driver:\n    artifact: {DRIVER}\n",
        lifecycle = capsule.lifecycle,
        top = capsule.top,
        inference = capsule.inference,
    )
}

struct Project {
    dir: TempDir,
    manifest: PathBuf,
}

fn project(manifest_yaml: &str) -> Project {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(&manifest, manifest_yaml).unwrap();
    Project { dir, manifest }
}

/// What one `mur run` left behind, and when it started.
struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
    started: SystemTime,
}

impl Run {
    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn trace_path(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout).join("trace.jsonl")
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(&self.trace_path())
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }
}

/// `mur run --task … --verbose` on `project`, with `extra` arguments.
fn run(home: &TempDir, project: &Project, extra: &[&str]) -> Run {
    let started = SystemTime::now();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MURMUR_A2A_TIMEOUT_SECS", "1")
        .current_dir(project.dir.path())
        .args([
            "run",
            "--manifest",
            project.manifest.to_str().unwrap(),
            "--task",
            TASK,
            "--verbose",
        ])
        .args(extra)
        .timeout(Duration::from_secs(300))
        .output()
        .unwrap();
    Run {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        output,
        started,
    }
}

// ── Reading the trace ─────────────────────────────────────────────────────────

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn first<'a>(trace: &'a [Value], event_type: &str) -> &'a Value {
    events(trace, event_type)
        .first()
        .copied()
        .unwrap_or_else(|| panic!("no {event_type} in {trace:#?}"))
}

fn position(trace: &[Value], event_type: &str) -> usize {
    trace
        .iter()
        .position(|event| event["event_type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in {trace:#?}"))
}

fn ms(event: &Value) -> i64 {
    event["timestamp"].as_i64().unwrap()
}

/// `at`, RFC 3339, as milliseconds since the epoch.
fn rfc3339_ms(at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .unwrap_or_else(|error| panic!("'{at}' is not RFC 3339: {error}"))
        .timestamp_millis()
}

fn epoch_ms(time: SystemTime) -> i64 {
    i64::try_from(time.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn assert_within(actual: i64, expected: i64, tolerance: i64, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} is not within {tolerance} of {expected}"
    );
}

/// Sum `key` over `records`, `None` when no record carries it.
fn sum(records: &[&Value], key: &str) -> Option<u64> {
    records
        .iter()
        .filter_map(|record| record[key].as_u64())
        .fold(None, |total, n| Some(total.unwrap_or(0) + n))
}

/// The raw text of the tool result the runtime fed back for `tool_id`.
fn raw_tool_result(requests: &[Value], tool_id: &str) -> String {
    let block = common::find_tool_result(requests, tool_id)
        .unwrap_or_else(|| panic!("no tool result for {tool_id}"));
    common::extract_result_text(&block)
}

/// `text` with the deadline notice line removed, and the notice line itself.
fn split_notice(text: &str) -> (String, Option<String>) {
    match text.rsplit_once('\n') {
        Some((rest, last)) if last.starts_with(NOTICE_PREFIX) => {
            (rest.to_string(), Some(last.to_string()))
        }
        _ => (text.to_string(), None),
    }
}

// ── In the tool list ──────────────────────────────────────────────────────

/// Every http agent capsule is offered `session-status`, with the fixed description and an
/// empty-object schema, granted nothing; `session_start` lists it.
#[test]
fn session_status_is_in_every_agent_capsules_tool_list() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(vec![end_turn()]);
    let project = project(&http_manifest(&server.endpoint, &Http::default()));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let requests = server.requests();
    let tools = requests[0]["tools"].as_array().expect("a tool array");
    let offered: Vec<&Value> = tools
        .iter()
        .filter(|tool| tool["name"] == "session-status")
        .collect();
    assert_eq!(offered.len(), 1, "{tools:#?}");
    assert_eq!(
        offered[0]["description"].as_str(),
        Some(capsule_runtime::session_status::description_http().as_str()),
        "{tools:#?}"
    );
    assert_eq!(
        offered[0]["input_schema"],
        json!({"type": "object", "properties": {}}),
        "{tools:#?}"
    );

    let trace = run.trace();
    let declared = first(&trace, "session_start")["tools_declared"].clone();
    assert!(
        declared
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "session-status"),
        "{declared}"
    );
    assert_eq!(first(&trace, "session_start")["deadline"], Value::Null);
    assert!(events(&trace, "session_status").is_empty());
}

// ── The fields match the trace ────────────────────────────────────────────

/// Turn 1 runs a granted tool, turn 2 calls `session-status`, turn 3 ends. Every field the model
/// was handed agrees with the trace written before the call, and the `session_status` record
/// carries the same object.
#[test]
fn session_status_fields_match_the_trace() {
    if common::skip_without_host_support("session_status_fields_match_the_trace") {
        return;
    }
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(vec![
        tool_turn("toolu_1", "sleep", json!({"command": "1"})),
        tool_turn("toolu_2", "session-status", json!({})),
        end_turn(),
    ]);
    let project = project(&http_manifest(
        &server.endpoint,
        &Http {
            sleep: true,
            inference: "  max_turns: 20\n  max_session_tokens: 1000000\n",
            top: "context:\n  max_tokens: 50000\n",
            ..Http::default()
        },
    ));
    let run = run(&home, &project, &["--deadline", "600"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let trace = run.trace();
    let at = position(&trace, "session_status");
    let record = &trace[at];
    let status = &record["status"];
    let handed: Value = serde_json::from_str(
        &common::tool_result_text(&server.requests(), "toolu_2").expect("the turn-3 request"),
    )
    .unwrap();
    assert_eq!(
        &handed, status,
        "the record is the object the model was handed"
    );

    let task_id = first(&trace, "task_start")["task_id"].clone();
    let before: Vec<&Value> = trace[..at]
        .iter()
        .filter(|event| event["event_type"] == "inference" && event["task_id"] == task_id)
        .collect();
    assert_eq!(before.len(), 2, "{trace:#?}");
    assert_eq!(status["turns"], json!({"used": 2, "cap": 20}));

    assert_eq!(
        status["tokens"],
        json!({
            "input": sum(&before, "input_tokens_actual").unwrap(),
            "output": sum(&before, "output_tokens_actual").unwrap(),
            "cached": sum(&before, "cached_tokens").unwrap(),
            "cache_write": sum(&before, "cache_write_tokens").unwrap(),
            "unreported_calls": 0,
        })
    );

    // Every scripted call reports usage, so the context and the spend are the provider's counts:
    // the request (`input_tokens_actual` with both cache members) plus the completion.
    let provider_count = |record: &Value| {
        [
            "input_tokens_actual",
            "cached_tokens",
            "cache_write_tokens",
            "output_tokens_actual",
        ]
        .iter()
        .map(|key| record[*key].as_u64().unwrap())
        .sum::<u64>()
    };
    assert_eq!(
        status["context"],
        json!({
            "used_tokens": provider_count(before.last().unwrap()),
            "window_tokens": 50000,
            "measured_by": "actual",
        })
    );

    let spent: u64 = before.iter().map(|record| provider_count(record)).sum();
    assert_eq!(
        status["spend"],
        json!({"tokens": spent, "ceiling": 1000000, "machine": "none"})
    );

    let deadline_at = rfc3339_ms(status["time"]["deadline"]["at"].as_str().unwrap());
    assert_within(
        deadline_at,
        epoch_ms(run.started) + 600_000,
        2_000,
        "deadline.at against mur run's start plus 600 s",
    );
    let left = status["time"]["deadline"]["left_secs"].as_i64().unwrap() * 1000;
    assert_within(left, deadline_at - ms(record), 2_000, "left_secs");
    let elapsed = status["time"]["elapsed_secs"].as_i64().unwrap() * 1000;
    assert_within(
        elapsed,
        ms(record) - ms(first(&trace, "session_start")),
        2_000,
        "elapsed_secs",
    );
    let task_elapsed = status["time"]["task_elapsed_secs"].as_i64().unwrap() * 1000;
    assert_within(
        task_elapsed,
        ms(record) - ms(first(&trace, "task_start")),
        2_000,
        "task_elapsed_secs",
    );
    assert_eq!(record["task_id"], task_id);

    let start_deadline = &first(&trace, "session_start")["deadline"];
    assert_eq!(start_deadline["secs"], 600);
    assert_eq!(start_deadline["notice_secs"], 60);
    assert_eq!(start_deadline["at"], status["time"]["deadline"]["at"]);
    assert!(events(&trace, "deadline_notice").is_empty(), "{trace:#?}");
}

// ── Unset limits and unreported counts ────────────────────────────────────

/// No deadline, no turn cap, no ceiling, no window and a provider that reports no usage: every
/// limit reads `"none"` and every count the runtime lacks `"unknown"`.
#[test]
fn unset_limits_say_none_and_unreported_counts_say_unknown() {
    let home = home_with_driver("");
    let server = common::ScriptedServer::start(vec![
        response(
            json!([{"type": "tool_use", "id": "toolu_1", "name": "session-status", "input": {}}]),
            "tool_use",
            None,
        ),
        response(json!([{"type": "text", "text": "done"}]), "end_turn", None),
    ]);
    let project = project(&http_manifest(
        &server.endpoint,
        &Http {
            inference: "  max_tokens: 1000\n",
            ..Http::default()
        },
    ));
    let run = run(&home, &project, &[]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let status = first(&run.trace(), "session_status")["status"].clone();
    assert_eq!(status["time"]["deadline"], "none");
    assert_eq!(status["turns"]["cap"], "none");
    assert_eq!(status["turns"]["used"], 1);
    assert_eq!(status["context"]["window_tokens"], "none");
    assert_eq!(status["context"]["measured_by"], "estimate");
    assert_eq!(
        status["tokens"],
        json!({"input": "unknown", "output": "unknown", "cached": "unknown",
               "cache_write": "unknown", "unreported_calls": 1})
    );
    assert_eq!(status["spend"]["ceiling"], "none");
    assert_eq!(status["spend"]["machine"], "none");
}

// ── Process transport ─────────────────────────────────────────────────────

/// A process capsule whose fake harness runs `profile`, with `wait` seconds before its third
/// call.
fn process_project(profile: &str, wait: u64, lifecycle: &str) -> (TempDir, Project) {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        PROCESS_DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    publish_compiled(&home, &driver);

    let dir = TempDir::new().unwrap();
    let harness = dir.path().join("harness.sh");
    fs::write(
        &harness,
        format!(
            "#!/bin/bash\nexport FIXTURE_HARNESS_PROFILE={profile}\nexport \
             FIXTURE_STATUS_WAIT={wait}\nexec '{}' \"$@\"\n",
            common::fixture_path("process-driver/fake-harness").display()
        ),
    )
    .unwrap();
    let mut perms = fs::metadata(&harness).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&harness, perms).unwrap();

    let manifest = dir.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: status-process\nversion: 0.1.0\nartifacts:\n  - name: {PROCESS_DRIVER}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  env:\n    allow: \
             [HOME, PATH, FIXTURE_HARNESS_PROFILE]\nlifecycle:\n  task_acceptance: \
             single\n{lifecycle}inference:\n  transport: process\n  max_turns: 10\n  driver:\n    \
             artifact: {PROCESS_DRIVER}\n  command: {}\n",
            harness.display()
        ),
    )
    .unwrap();
    (home, Project { dir, manifest })
}

/// The text a bridged `tools/call` answer carried, from the harness's copy of the response.
fn bridged_text(workdir: &Path, name: &str) -> String {
    let path = common::find_file(workdir, name)
        .unwrap_or_else(|| panic!("the harness kept no {name} under {workdir:?}"));
    let body: Value = serde_json::from_str(fs::read_to_string(path).unwrap().trim()).unwrap();
    body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {body}"))
        .to_string()
}

/// Through the bridge, `session-status` reports no context, the turns the harness has closed and
/// the harness's own token counts; one later bridged result carries the notice.
#[cfg(unix)]
#[test]
fn session_status_through_the_process_bridge() {
    // The notice falls due 30 seconds after `mur run` starts. The first two calls come right after
    // staging, the third 28 seconds later, so the notice lands on the third unless staging alone
    // took the whole 30.
    let (home, project) = process_project("session-status", 28, "  deadline_notice_secs: 90\n");
    let run = run(&home, &project, &["--deadline", "120"]);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let trace = run.trace();
    let statuses: Vec<(usize, &Value)> = trace
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event_type"] == "session_status")
        .collect();
    assert_eq!(statuses.len(), 3, "{trace:#?}");
    let workdir = run.workdir();
    let mut carriers = Vec::new();
    for (n, (at, record)) in statuses.iter().enumerate() {
        let status = &record["status"];
        assert_eq!(status["context"], "unknown", "{status}");
        assert_eq!(status["turns"]["cap"], 10);
        let before: Vec<&Value> = trace[..*at]
            .iter()
            .filter(|event| event["event_type"] == "inference")
            .collect();
        assert_eq!(status["turns"]["used"], before.len(), "{trace:#?}");
        for (key, member) in [
            ("input_tokens", "input"),
            ("output_tokens", "output"),
            ("cached_tokens", "cached"),
            ("cache_write_tokens", "cache_write"),
        ] {
            let expected = sum(&before, key).map_or(json!("unknown"), Value::from);
            assert_eq!(status["tokens"][member], expected, "{member}: {trace:#?}");
        }
        let text = bridged_text(&workdir, &format!("status-{}.answer", n + 1));
        let (fenced, notice) = split_notice(&text);
        let handed: Value = serde_json::from_str(&common::unfence(&fenced)).unwrap();
        assert_eq!(&handed, status, "the record is what the harness was handed");
        if notice.is_some() {
            carriers.push((n, status["time"]["deadline"]["left_secs"].as_u64().unwrap()));
        }
    }
    // The second call sees the first turn closed, with the harness's counts.
    let second = &statuses[1].1["status"];
    assert_eq!(second["turns"]["used"], 1, "{trace:#?}");
    assert_eq!(second["tokens"]["input"], 40);
    assert_eq!(second["tokens"]["cached"], 100);

    assert_eq!(
        carriers.len(),
        1,
        "exactly one bridged result carries the notice"
    );
    let (carrier, left) = carriers[0];
    assert!(left <= 91, "the notice is due within the margin: {left}");
    for (_, earlier) in &statuses[..carrier] {
        assert!(
            earlier["status"]["time"]["deadline"]["left_secs"]
                .as_u64()
                .unwrap()
                >= 89,
            "an earlier result inside the margin would have carried it: {earlier}"
        );
    }
    let notices = events(&trace, "deadline_notice");
    assert_eq!(notices.len(), 1, "{trace:#?}");
    assert_eq!(notices[0]["tool_name"], "session-status", "{trace:#?}");
}

// ── One notice ────────────────────────────────────────────────────────────

/// The scripted conversation of the notice case: `sleep 1`, `sleep 40`, `session-status`,
/// `sleep 1`, then the end.
fn notice_script() -> Vec<String> {
    vec![
        tool_turn("toolu_1", "sleep", json!({"command": "1"})),
        tool_turn("toolu_2", "sleep", json!({"command": "40"})),
        tool_turn("toolu_3", "session-status", json!({})),
        tool_turn("toolu_4", "sleep", json!({"command": "1"})),
        end_turn(),
    ]
}

const NOTICE_TOOL_IDS: [&str; 4] = ["toolu_1", "toolu_2", "toolu_3", "toolu_4"];

struct NoticeRun {
    run: Run,
    requests: Vec<Value>,
    /// The scripted provider's address, which the capsule's network allowlist names.
    endpoint: String,
    _home: TempDir,
    _project: Project,
}

fn notice_run(lifecycle: &'static str, extra: &'static [&'static str]) -> NoticeRun {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(notice_script());
    let project = project(&http_manifest(
        &server.endpoint,
        &Http {
            sleep: true,
            lifecycle,
            ..Http::default()
        },
    ));
    let run = run(&home, &project, extra);
    NoticeRun {
        run,
        requests: server.requests(),
        endpoint: server.endpoint.clone(),
        _home: home,
        _project: project,
    }
}

/// With 40 seconds of margin on a 75-second deadline, exactly one tool result carries the notice
/// — the first returned inside the margin — and nothing else the conversation holds differs from
/// the same run with no deadline; a margin of `0` sends none.
///
/// The deadline counts from `mur run`'s start, staging included, so the numbers leave a loaded
/// host room to stage: the 40-second sleep always ends inside the margin, and the run ends well
/// before the deadline. Which result carries the notice is read from the trace's timestamps.
#[test]
fn the_deadline_notice_is_sent_once() {
    if common::skip_without_host_support("the_deadline_notice_is_sent_once") {
        return;
    }
    let with =
        std::thread::spawn(|| notice_run("  deadline_notice_secs: 40\n", &["--deadline", "75"]));
    let without = std::thread::spawn(|| notice_run("  deadline_notice_secs: 40\n", &[]));
    let silent =
        std::thread::spawn(|| notice_run("  deadline_notice_secs: 0\n", &["--deadline", "75"]));
    let (with, without, silent) = (
        with.join().unwrap(),
        without.join().unwrap(),
        silent.join().unwrap(),
    );
    for each in [&with, &without, &silent] {
        assert_eq!(
            each.run.output.status.code(),
            Some(0),
            "{}",
            each.run.context()
        );
    }

    let trace = with.run.trace();
    let deadline_at = first(&trace, "session_start")["deadline"]["at"]
        .as_str()
        .unwrap()
        .to_string();
    let deadline_ms = rfc3339_ms(&deadline_at);
    let tool_calls = events(&trace, "tool_call");
    assert_eq!(tool_calls.len(), 4, "{trace:#?}");

    let mut carrier = None;
    for (n, id) in NOTICE_TOOL_IDS.iter().enumerate() {
        let (_, notice) = split_notice(&raw_tool_result(&with.requests, id));
        if let Some(notice) = notice {
            assert!(carrier.is_none(), "a second tool result carried the notice");
            let left_secs = notice
                .strip_prefix(&format!("{NOTICE_PREFIX}{deadline_at}, in "))
                .and_then(|rest| rest.strip_suffix(" seconds."))
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or_else(|| panic!("the notice line is malformed: {notice}"));
            assert!(left_secs <= 40, "{notice}");
            carrier = Some(n);
        }
    }
    let carrier = carrier.expect("one tool result carries the notice");
    let left_at = |n: usize| deadline_ms - ms(tool_calls[n]);
    assert!(
        left_at(carrier) <= 41_000,
        "carried {}ms out",
        left_at(carrier)
    );
    for earlier in 0..carrier {
        assert!(
            left_at(earlier) > 39_000,
            "result {earlier} came back inside the margin ({}ms) without the notice",
            left_at(earlier)
        );
    }
    let notices = events(&trace, "deadline_notice");
    assert_eq!(notices.len(), 1, "{trace:#?}");
    assert_eq!(notices[0]["tool_name"], tool_calls[carrier]["tool_name"]);
    assert_eq!(notices[0]["deadline_at"], deadline_at.as_str());
    assert_eq!(notices[0]["notice_secs"], 40);
    assert_eq!(events(&trace, "session_status").len(), 1);

    // Nothing else was added: the same requests, message for message, as the run with no
    // deadline, and every result but `session-status`'s the same text once the notice is gone.
    assert_eq!(with.requests.len(), without.requests.len());
    for (a, b) in with.requests.iter().zip(&without.requests) {
        assert_eq!(
            a["messages"].as_array().unwrap().len(),
            b["messages"].as_array().unwrap().len()
        );
        // Each run has its own provider, and the system text names its address.
        let own =
            |run: &NoticeRun, part: &Value| part.to_string().replace(&run.endpoint, "<provider>");
        assert_eq!(
            own(&with, &a["system"]),
            own(&without, &b["system"]),
            "the system prompt is unchanged"
        );
        assert_eq!(
            own(&with, &a["tools"]),
            own(&without, &b["tools"]),
            "the tool array is unchanged"
        );
    }
    for id in ["toolu_1", "toolu_2", "toolu_4"] {
        let (text, _) = split_notice(&raw_tool_result(&with.requests, id));
        assert_eq!(text, raw_tool_result(&without.requests, id), "{id}");
    }

    let silent_trace = silent.run.trace();
    assert!(events(&silent_trace, "deadline_notice").is_empty());
    assert_eq!(
        first(&silent_trace, "session_start")["deadline"]["notice_secs"],
        0
    );
    for id in NOTICE_TOOL_IDS {
        assert!(
            !raw_tool_result(&silent.requests, id).contains(NOTICE_PREFIX),
            "a margin of 0 sends no notice"
        );
    }

    // `mur trace show` renders the deadline and each new record on one line.
    let show = Command::cargo_bin("mur")
        .unwrap()
        .args(["trace", "show"])
        .arg(with.run.trace_path())
        .output()
        .unwrap();
    let show = String::from_utf8_lossy(&show.stdout);
    assert!(
        show.contains(&format!(
            "deadline:   {deadline_at}  (75s after mur run started; notice 40s before)"
        )),
        "{show}"
    );
    assert_eq!(
        show.lines()
            .filter(|line| line.starts_with("session_status  "))
            .count(),
        1,
        "{show}"
    );
    assert_eq!(
        show.lines()
            .filter(|line| line.starts_with("deadline_notice  "))
            .count(),
        1,
        "{show}"
    );
}

// ── The deadline ends the session ─────────────────────────────────────────

/// The deadline the ending cases run under: long enough that staging is over well before it, so
/// the session is in its first turn when it falls.
const DEADLINE_SECS: u64 = 25;

/// `deadline_reached`, then the wind-down, then `session_end` saying so, and `E-RUN-053`.
///
/// A host loaded enough to spend the whole deadline staging reaches it before the task starts,
/// which ends the session with no task run at all; that is asserted instead.
fn assert_ended_by_the_deadline(run: &Run) {
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.stderr.contains("error[E-RUN-053]"), "{}", run.context());
    assert!(
        run.stdout.contains("status:  deadline_reached"),
        "{}",
        run.context()
    );
    let trace = run.trace();
    let reached = position(&trace, "deadline_reached");
    let session_end = position(&trace, "session_end");
    assert_eq!(session_end, trace.len() - 1, "{trace:#?}");
    assert_eq!(trace[session_end]["exit_status"], "deadline_reached");
    assert_eq!(trace[reached]["deadline_secs"], DEADLINE_SECS);
    if events(&trace, "task_start").is_empty() {
        assert!(reached < session_end, "{trace:#?}");
        return;
    }
    let task_end = position(&trace, "task_end");
    assert!(reached < task_end && task_end < session_end, "{trace:#?}");
    assert!(
        trace[..reached]
            .iter()
            .all(|event| event["event_type"] != "task_canceled"),
        "deadline_reached precedes every cancel: {trace:#?}"
    );
    assert_eq!(trace[task_end]["exit_status"], "canceled");
    assert_eq!(
        trace[reached]["deadline_at"],
        first(&trace, "session_start")["deadline"]["at"]
    );
}

/// A turn whose provider never answers is ended by the deadline.
#[test]
fn the_deadline_ends_an_http_session_in_flight() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let endpoint = common::never_replying();
    let project = project(&http_manifest(&endpoint, &Http::default()));
    let begun = Instant::now();
    let run = run(&home, &project, &["--deadline", &DEADLINE_SECS.to_string()]);
    assert!(
        begun.elapsed() < Duration::from_secs(60),
        "{}",
        run.context()
    );
    assert_ended_by_the_deadline(&run);
}

/// A harness that never answers is ended by the deadline, and is gone afterwards.
#[cfg(unix)]
#[test]
fn the_deadline_ends_a_process_session_and_its_harness() {
    let (home, project) = process_project("silent", 0, "");
    let run = run(&home, &project, &["--deadline", &DEADLINE_SECS.to_string()]);
    assert_ended_by_the_deadline(&run);
    if events(&run.trace(), "task_start").is_empty() {
        return;
    }
    let pid = fs::read_to_string(
        common::find_file(&run.workdir(), "harness.pid").expect("the harness wrote its pid"),
    )
    .unwrap()
    .trim()
    .parse::<i32>()
    .unwrap();
    common::assert_dead_within(pid, Duration::from_secs(5));
}

// ── Validation ────────────────────────────────────────────────────────────

fn refused(home: &TempDir, args: &[&str]) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(args)
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0), "{args:?} must be refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `--deadline` takes a whole number of seconds from 1 to ten years, needs an agent capsule, and is
/// not a `--roster` flag.
#[test]
fn deadline_arguments_are_validated() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let agent = project(&http_manifest("http://127.0.0.1:9", &Http::default()));
    let manifest = agent.manifest.to_str().unwrap();
    for bad in [
        "0",
        "1.5",
        "soon",
        "-3",
        "315360001",
        "18446744073709551615",
    ] {
        let stderr = refused(
            &home,
            &[
                "run",
                "--manifest",
                manifest,
                "--task",
                TASK,
                &format!("--deadline={bad}"),
            ],
        );
        assert!(stderr.contains("--deadline"), "{bad}: {stderr}");
    }
    assert!(!agent.dir.path().join("workdir").exists());

    let script = project("name: script\nversion: 0.1.0\nartifacts: []\n");
    let stderr = refused(
        &home,
        &[
            "run",
            "--manifest",
            script.manifest.to_str().unwrap(),
            "--deadline",
            "30",
        ],
    );
    assert!(
        stderr.contains("--deadline requires an agent capsule"),
        "{stderr}"
    );

    let stderr = refused(&home, &["run", "--roster", "--deadline", "30"]);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

/// Only the launch that owns its process enforces a deadline; `launch_session` refuses a session
/// staged with one before anything runs.
#[test]
fn launch_session_refuses_a_staged_deadline() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let agent = project(&http_manifest("http://127.0.0.1:9", &Http::default()));
    let staged = common::stage_agent_session_with_deadline(
        &home,
        agent.dir.path(),
        &agent.manifest,
        capsule_runtime::session_status::SessionDeadline::after(
            Instant::now(),
            SystemTime::now(),
            30,
        ),
    );
    let refused = capsule_runtime::launch_session(staged, |_| {}).unwrap_err();
    assert!(
        matches!(
            refused,
            capsule_runtime::RuntimeError::SessionDeadlineUnenforceable { .. }
        ),
        "{refused:?}"
    );
}
