//! Integration tests for programmatic launch (--json flag).

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "json-launch-agent";
const CAPSULE_VERSION: &str = "0.1.0";

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()])
}

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );

    // Write project files first so find_project_root() works during install.
    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: json-launch-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    // Install into the global store (used by stage_agent / direct API tests).
    common::publish_local(&home, &driver_artifact).success();
    // Install into the project store (used by `mur run` CLI tests).
    common::install_artifact_to_project(project.path(), &driver_artifact).success();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = std::collections::HashSet::new();
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
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
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

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();

    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    let mut body = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        body.push_str(&line);
        line.clear();
    }

    (status, body)
}

/// Build the same JSON string that `mur run --json` emits, using the provided values.
fn build_json_line(
    url: &str,
    pid: u32,
    session_id: &str,
    name: &str,
    version: &str,
    workdir: &Path,
) -> String {
    serde_json::json!({
        "url": url,
        "pid": pid,
        "session_id": session_id,
        "name": name,
        "version": version,
        "workdir": workdir.to_string_lossy(),
    })
    .to_string()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Test 1: The on_url closure produces a valid JSON line with the required fields.
#[test]
fn json_launch_emits_parseable_json() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);

    let expected_session_id = staged.session_id.clone();
    let expected_pid = std::process::id();
    let expected_workdir = staged.workdir.clone();

    let (json_tx, json_rx) = std::sync::mpsc::channel::<String>();

    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let line = build_json_line(
                url,
                expected_pid,
                &expected_session_id,
                CAPSULE_NAME,
                CAPSULE_VERSION,
                &expected_workdir,
            );
            let _ = json_tx.send(line);
        })
        .expect("launch should succeed")
    });

    let json_line = json_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for JSON line");

    handle.join().expect("launch thread should not panic");

    let parsed: Value =
        serde_json::from_str(&json_line).expect("JSON line should parse as valid JSON");

    let url = parsed["url"].as_str().unwrap_or("");
    assert!(
        !url.is_empty(),
        "url field should be non-empty; got: {parsed}"
    );
    assert!(
        url.starts_with("localhost:"),
        "url should start with 'localhost:'; got: '{url}'"
    );

    let pid = parsed["pid"].as_u64().unwrap_or(0);
    assert!(
        pid > 0,
        "pid field should be a positive integer; got: {parsed}"
    );

    let sid = parsed["session_id"].as_str().unwrap_or("");
    assert!(
        !sid.is_empty(),
        "session_id field should be non-empty; got: {parsed}"
    );

    let name = parsed["name"].as_str().unwrap_or("");
    assert!(
        !name.is_empty(),
        "name field should be non-empty; got: {parsed}"
    );

    let version = parsed["version"].as_str().unwrap_or("");
    assert!(
        !version.is_empty(),
        "version field should be non-empty; got: {parsed}"
    );

    let wd = parsed["workdir"].as_str().unwrap_or("");
    assert!(
        !wd.is_empty(),
        "workdir field should be non-empty; got: {parsed}"
    );
    assert!(
        std::path::Path::new(wd).is_absolute(),
        "workdir should be an absolute path; got: '{wd}'"
    );
}

/// Test 2: The URL in the JSON line is live and serves the A2A agent card.
#[test]
fn json_launch_url_is_reachable() {
    let server = end_turn_server("agent card test done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);

    fs::write(staged.workdir.join("task.md"), "agent card test").unwrap();

    let expected_session_id = staged.session_id.clone();
    let expected_pid = std::process::id();
    let expected_workdir = staged.workdir.clone();

    let (json_tx, json_rx) = std::sync::mpsc::channel::<String>();

    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let line = build_json_line(
                url,
                expected_pid,
                &expected_session_id,
                CAPSULE_NAME,
                CAPSULE_VERSION,
                &expected_workdir,
            );
            let _ = json_tx.send(line);
        })
        .expect("launch should succeed")
    });

    let json_line = json_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for JSON line");

    let parsed: Value =
        serde_json::from_str(&json_line).expect("JSON line should parse as valid JSON");
    let url = parsed["url"]
        .as_str()
        .expect("url field should be a string");

    let (status, body) = http_get(url, "/.well-known/agent-card.json");
    assert_eq!(
        status, 200,
        "agent card endpoint should return HTTP 200; got {status}"
    );

    let card: Value = serde_json::from_str(&body).expect("agent card body should be valid JSON");
    assert!(
        card.get("name").is_some(),
        "agent card should have a 'name' field; got: {card}"
    );
    assert!(
        card.get("version").is_some(),
        "agent card should have a 'version' field; got: {card}"
    );

    handle.join().expect("launch thread should not panic");
}

/// Test 3: The session_id in the JSON matches the session_start event in trace.jsonl.
#[test]
fn json_launch_session_id_matches_trace() {
    let server = end_turn_server("trace correlation test");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);

    fs::write(staged.workdir.join("task.md"), "trace test").unwrap();

    let expected_session_id = staged.session_id.clone();
    let expected_pid = std::process::id();
    let workdir = staged.workdir.clone();
    let expected_workdir = workdir.clone();

    let (json_tx, json_rx) = std::sync::mpsc::channel::<String>();

    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let line = build_json_line(
                url,
                expected_pid,
                &expected_session_id,
                CAPSULE_NAME,
                CAPSULE_VERSION,
                &expected_workdir,
            );
            let _ = json_tx.send(line);
        })
        .expect("launch should succeed")
    });

    let json_line = json_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("timed out waiting for JSON line");

    handle.join().expect("launch thread should not panic");

    let parsed: Value =
        serde_json::from_str(&json_line).expect("JSON line should parse as valid JSON");
    let json_session_id = parsed["session_id"]
        .as_str()
        .expect("session_id should be a string")
        .to_string();

    let trace_path = workdir.join("trace.jsonl");
    assert!(
        trace_path.exists(),
        "trace.jsonl should exist at {}",
        trace_path.display()
    );

    let trace_content = fs::read_to_string(&trace_path).expect("should read trace.jsonl");
    let session_start_event = trace_content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["event_type"].as_str() == Some("session_start"))
        .expect("trace.jsonl should contain a session_start event");

    let trace_session_id = session_start_event["session_id"]
        .as_str()
        .expect("session_start event should have a session_id field");

    assert_eq!(
        json_session_id, trace_session_id,
        "session_id in JSON output should match session_id in trace.jsonl session_start event"
    );
}

/// Test 4: Without --json, human-readable startup and completion lines appear on stdout.
///
/// `murmur: url` and `session:` are always emitted in the startup block.
/// `workdir:` and the other extended fields appear in the startup block only with --verbose.
/// `status:` is emitted at completion.
///
/// Uses an agent capsule (inference-based) because `murmur: url` is only emitted
/// for agent capsules (script capsules do not start the HTTP server).
#[test]
fn no_json_flag_human_readable_output_unchanged() {
    let server = end_turn_server("human readable test done");
    let artifacts = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    // Write task.md directly into the project dir so mur run picks it up.
    // The manifest_path is <project>/murmur.yaml; workdir is created later.
    // We must provide task via --task flag since workdir doesn't exist yet.
    let input_file = artifacts.path().join("task.md");
    fs::write(&input_file, "human readable output test").unwrap();

    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            input_file.to_str().unwrap(),
            "--verbose",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();

    // `session:` and `murmur: url` appear in the startup block (always).
    // `workdir:` appears in the startup block when --verbose is set.
    assert!(
        stdout.contains("murmur: url "),
        "stdout should contain 'murmur: url '; got:\n{stdout}"
    );
    assert!(
        stdout.contains("session: "),
        "stdout should contain 'session: '; got:\n{stdout}"
    );
    assert!(
        stdout.contains("workdir: "),
        "stdout should contain 'workdir: '; got:\n{stdout}"
    );

    // Verify no JSON line is present
    let has_json_line = stdout.lines().any(|line| {
        let t = line.trim();
        t.starts_with('{') && serde_json::from_str::<Value>(t).is_ok()
    });
    assert!(
        !has_json_line,
        "stdout should not contain a JSON line without --json; got:\n{stdout}"
    );
}

/// Test 5: --workdir sets the workdir field in JSON output and creates .murmur/ inside it.
#[test]
fn json_launch_workdir_flag_is_reflected() {
    let server = end_turn_server("workdir flag test done");
    let artifacts = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let user_workdir = tempfile::tempdir().unwrap();

    let input_file = artifacts.path().join("task.md");
    fs::write(&input_file, "workdir flag test").unwrap();

    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            input_file.to_str().unwrap(),
            "--workdir",
            user_workdir.path().to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    let json_line = stdout
        .lines()
        .find(|line| {
            let t = line.trim();
            t.starts_with('{') && serde_json::from_str::<Value>(t).is_ok()
        })
        .expect("should have a JSON line in output");

    let parsed: Value =
        serde_json::from_str(json_line).expect("JSON line should parse as valid JSON");

    let wd = parsed["workdir"]
        .as_str()
        .expect("workdir field should be a string");
    let expected = user_workdir.path().to_str().unwrap();
    assert_eq!(
        wd, expected,
        "workdir in JSON should match the --workdir argument"
    );
    assert!(
        std::path::Path::new(wd).is_absolute(),
        "workdir should be an absolute path; got: '{wd}'"
    );

    // .murmur/ directory should have been created inside the workdir
    let murmur_dir = user_workdir.path().join(".murmur");
    assert!(
        murmur_dir.exists() && murmur_dir.is_dir(),
        ".murmur/ directory should exist inside --workdir; checked: {}",
        murmur_dir.display()
    );
}

// ── A supervisor that stops reading ───────────────────────────────────────────
//
// `mur run --json` writes one readiness line and the supervisor that asked for it is entitled to
// stop reading there. The write then fails with `EPIPE`, because Rust leaves `SIGPIPE` at
// `SIG_IGN`. A bare `println!` panics on that error, and the release profile's `panic = "abort"`
// turns the panic into `SIGABRT` at the print site — the capsule dies the instant it succeeded.
//
// `cargo test` builds the `dev` profile, where a panic unwinds instead: the same defect shows up
// as exit status 101 and `panicked at` on stderr. These tests assert on both shapes — a clean
// exit code, no terminating signal, and no panic text — so they catch it under either profile.

/// Launch `mur run --json` as a live child with both streams piped. Without a task file the
/// capsule waits for one at its door.
fn spawn_json_run(
    home: &TempDir,
    manifest_path: &Path,
    task_file: Option<&Path>,
    workdir: Option<&Path>,
) -> std::process::Child {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["run", "--manifest", manifest_path.to_str().unwrap()]);
    if let Some(task_file) = task_file {
        command.args(["--task", task_file.to_str().unwrap()]);
    }
    if let Some(dir) = workdir {
        command.args(["--workdir", dir.to_str().unwrap()]);
    }
    command
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("mur should spawn")
}

/// Drain a piped stream on its own thread, so the child never blocks on a full pipe.
fn drain_on_thread(stream: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let mut reader = BufReader::new(stream);
        let mut chunk = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut chunk).ok();
        text.push_str(&String::from_utf8_lossy(&chunk));
        text
    })
}

/// The whole claim: the session ended on its own terms, not at a print.
fn assert_capsule_survived(status: std::process::ExitStatus, stderr: &str) {
    assert!(
        !stderr.contains("panicked at"),
        "a write failure must not panic; child stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("failed printing to stdout"),
        "a write failure must not reach the `println!` panic path; child stderr was:\n{stderr}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            None,
            "the capsule must not be killed by a signal (SIGABRT means the print aborted the \
             process); child stderr was:\n{stderr}"
        );
    }
    assert_eq!(
        status.code(),
        Some(0),
        "the session should exit 0; child stderr was:\n{stderr}"
    );
}

/// S1: a supervisor reads the readiness line, stops reading, and the capsule lives.
#[test]
fn json_launch_survives_a_supervisor_that_stops_reading_stdout() {
    let server = end_turn_server("supervisor stops reading");
    let inputs = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let task_file = inputs.path().join("task.md");
    fs::write(&task_file, "supervisor stops reading").unwrap();

    let mut child = spawn_json_run(&home, &manifest_path, Some(&task_file), None);
    let stderr = drain_on_thread(child.stderr.take().expect("stderr should be piped"));

    let readiness_line = {
        let mut reader = BufReader::new(child.stdout.take().expect("stdout should be piped"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the readiness line should arrive");
        line
        // `reader` is dropped here, closing this end of the pipe: every later write from the
        // capsule to stdout fails with `EPIPE`.
    };

    let status = child.wait().expect("mur should exit");
    let stderr = stderr.join().expect("stderr drain should not panic");
    assert_capsule_survived(status, &stderr);

    let parsed: Value = serde_json::from_str(readiness_line.trim())
        .expect("the readiness line should parse as JSON");
    assert!(
        !parsed["url"].as_str().unwrap_or("").is_empty(),
        "the readiness line should carry a non-empty url; got: {parsed}"
    );
}

/// S2: the reader is gone before the line is written, and the session still runs to completion.
#[test]
fn json_launch_survives_a_reader_that_never_reads() {
    let server = end_turn_server("reader never reads");
    let inputs = tempfile::tempdir().unwrap();
    let user_workdir = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let task_file = inputs.path().join("task.md");
    fs::write(&task_file, "reader never reads").unwrap();

    let mut child = spawn_json_run(
        &home,
        &manifest_path,
        Some(&task_file),
        Some(user_workdir.path()),
    );
    let stderr = drain_on_thread(child.stderr.take().expect("stderr should be piped"));
    // Nothing is read: by the time the readiness line is written there is no reader left.
    drop(child.stdout.take().expect("stdout should be piped"));

    let status = child.wait().expect("mur should exit");
    let stderr = stderr.join().expect("stderr drain should not panic");
    assert_capsule_survived(status, &stderr);

    let session_dir = fs::read_dir(user_workdir.path().join(".murmur"))
        .expect("--workdir should hold a .murmur directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("trace.jsonl").is_file())
        .expect("the session should have written a trace");

    let trace = fs::read_to_string(session_dir.join("trace.jsonl")).expect("should read trace");
    assert!(
        trace
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["event_type"].as_str() == Some("task_end")),
        "the task should have run to completion; trace was:\n{trace}"
    );
}

/// S3: standard error is closed on a live session, and the readiness line still lands.
#[test]
fn json_launch_survives_a_closed_standard_error() {
    let server = end_turn_server("stderr closed");
    let inputs = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let task_file = inputs.path().join("task.md");
    fs::write(&task_file, "stderr closed").unwrap();

    let mut child = spawn_json_run(&home, &manifest_path, Some(&task_file), None);
    drop(child.stderr.take().expect("stderr should be piped"));

    let stdout = drain_on_thread(child.stdout.take().expect("stdout should be piped"));
    let status = child.wait().expect("mur should exit");
    let stdout = stdout.join().expect("stdout drain should not panic");

    assert_capsule_survived(status, "<stderr was closed by this test>");

    let readiness_line = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .expect("the readiness line should still reach stdout");
    let parsed: Value =
        serde_json::from_str(readiness_line.trim()).expect("the readiness line should parse");
    assert!(
        !parsed["url"].as_str().unwrap_or("").is_empty(),
        "the readiness line should carry a non-empty url; got: {parsed}"
    );
}

/// POST one JSON-RPC request to the capsule's door and return the parsed response body.
fn http_post_json(addr: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let request = format!(
        "POST / HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({ "_raw": response }))
}

/// The reported failure end to end: a supervisor reads the readiness line and closes the pipe, and
/// the capsule is still alive behind its door — it serves its agent card, accepts a task, runs it
/// and ends the session on its own terms.
#[test]
fn json_launch_answers_its_door_after_the_supervisor_stops_reading() {
    let server = end_turn_server("door still answers");
    let user_workdir = tempfile::tempdir().unwrap();
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let mut child = spawn_json_run(&home, &manifest_path, None, Some(user_workdir.path()));
    let stderr = drain_on_thread(child.stderr.take().expect("stderr should be piped"));

    let readiness_line = {
        let mut reader = BufReader::new(child.stdout.take().expect("stdout should be piped"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the readiness line should arrive");
        line
    };
    let parsed: Value = serde_json::from_str(readiness_line.trim())
        .expect("the readiness line should parse as JSON");
    let url = parsed["url"]
        .as_str()
        .expect("the readiness line should carry a url")
        .to_string();

    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the capsule should still be running after its supervisor closed stdout"
    );
    let (status, _) = http_get(&url, "/.well-known/agent-card.json");
    assert_eq!(status, 200, "the door should still serve the agent card");

    let response = http_post_json(
        &url,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {
                "messageId": "msg-1",
                "role": "user",
                "parts": [{"text": "door still answers"}]
            }}
        })
        .to_string(),
    );
    assert!(
        response["result"]["id"].as_str().is_some(),
        "the door should accept a task; got: {response}"
    );

    let status = child.wait().expect("mur should exit");
    let stderr = stderr.join().expect("stderr drain should not panic");
    assert_capsule_survived(status, &stderr);

    let session_dir = fs::read_dir(user_workdir.path().join(".murmur"))
        .expect("--workdir should hold a .murmur directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.join("trace.jsonl").is_file())
        .expect("the session should have written a trace");
    let trace = fs::read_to_string(session_dir.join("trace.jsonl")).expect("should read trace");
    assert!(
        trace
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["event_type"].as_str() == Some("task_end")),
        "the task submitted through the door should have run; trace was:\n{trace}"
    );
}
