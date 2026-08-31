#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn shell_execute_bash_and_reads_stdout() {
    if common::skip_without_host_support("shell_execute_bash_and_reads_stdout") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_ls",
                "name": "bash",
                "input": {"command": "ls"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "Directory listing received."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run ls and summarize it.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("Directory listing received."));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_ls").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.to_lowercase().contains("exit code"));
}

#[test]
fn shell_blocks_undeclared_binary() {
    if common::skip_without_host_support("shell_blocks_undeclared_binary") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_curl",
                "name": "curl",
                "input": {"command": "curl --version"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "text",
                "text": "tool 'curl' is not declared in manifest allowlist"
            }],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Try curl").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should not trap");
    let output = fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("curl"));
    assert!(output.contains("allowlist"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_curl").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.contains("curl"));
    assert!(text.contains("not declared in manifest allowlist"));
}

#[test]
fn shell_propagates_nonzero_exit_code() {
    if common::skip_without_host_support("shell_propagates_nonzero_exit_code") {
        return;
    }
    let server = ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_exit",
                "name": "bash",
                "input": {"command": "exit 42"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "Observed non-zero exit."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash"],
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.workdir.join("task.md"),
        "Run a failing bash command.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    assert!(launched.workdir.join("out/result.txt").exists());

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result_block(&requests[1], "toolu_exit").expect("tool_result");

    let text = tool_result_text(tool_result);
    assert!(text.contains("42"));
}

fn find_tool_result_block<'a>(request: &'a Value, tool_use_id: &str) -> Option<&'a Value> {
    let messages = request.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };

        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
            {
                return Some(block);
            }
        }
    }

    None
}

fn tool_result_text(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_string();
    }

    block
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
        })
        .unwrap_or_default()
}

fn stage_agent_session(home: &TempDir, project_dir: &Path, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn create_agent_project(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    shell_allow: &[&str],
) -> PathBuf {
    create_agent_project_with_lifecycle(project_dir, endpoint, driver_name, shell_allow, "")
}

/// `create_agent_project` with a `lifecycle:` block appended verbatim — `""` writes none, so a
/// capsule that declares nothing keeps the defaults.
fn create_agent_project_with_lifecycle(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    shell_allow: &[&str],
    lifecycle_yaml: &str,
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let shell_allow_yaml = shell_allow
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: agent-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver_name}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n{shell_allow_yaml}inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {driver_name}\n{lifecycle_yaml}"
        ),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

// ── Detached shell ────────────────────────────────────────────────────────────

/// One scripted `tool_use` response calling `bash` with `command`.
fn bash_call(id: &str, tool_use_id: &str, command: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": tool_use_id,
            "name": "bash",
            "input": {"command": command}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn trace_events(workdir: &Path) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("every trace line is valid JSON"))
        .collect()
}

fn events_of_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// Demotion, stated as a measurement: a command that outruns the grace period hands the turn a
/// handle, the turn keeps going and dispatches a second command while the first is still
/// running, and none of the first command's 200 000 output bytes reach the conversation.
#[test]
fn shell_detaches_a_slow_command_and_the_turn_continues() {
    if common::skip_without_host_support("shell_detaches_a_slow_command_and_the_turn_continues") {
        return;
    }
    let server = ScriptedServer::start(vec![
        // 200 000 output bytes from a bash builtin: only `bash` and `sleep` are declared, and
        // on a kernel-enforcement host nothing else is executable.
        bash_call("msg_1", "toolu_slow", "sleep 5; printf 'x%.0s' {1..200000}"),
        bash_call("msg_2", "toolu_second", "echo second"),
        end_turn("msg_3", "Both commands issued."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_project_with_lifecycle(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash", "sleep"],
        "lifecycle:\n  shell_grace_secs: 1\n",
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.workdir.join("task.md"),
        "Run a slow build, then say hi.",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the session must not wait on the detached command"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "three turns were scripted");

    let demotion = tool_result_text(
        find_tool_result_block(&requests[1], "toolu_slow").expect("the slow call has a result"),
    );
    assert!(
        demotion.len() < 400,
        "the demotion result is {} bytes, so it is carrying output:\n{demotion}",
        demotion.len()
    );
    assert!(
        !demotion.contains("xxxxx"),
        "none of the command's output may reach the turn:\n{demotion}"
    );
    assert!(
        demotion.to_lowercase().contains("background"),
        "the result says the command is still running:\n{demotion}"
    );

    let events = trace_events(&launched.workdir);
    let detached = events_of_type(&events, "shell_detached");
    assert_eq!(detached.len(), 1, "exactly one command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap();
    assert!(
        demotion.contains(work_id),
        "the demotion result names the work id {work_id}:\n{demotion}"
    );
    assert_eq!(detached[0]["grace_ms"], 1000);

    // The second command was dispatched and answered while the first was still running: its
    // `shell` record lands inside the window `shell_detached` opened, and the first command had
    // four of its five seconds left at that point.
    let foreground = events_of_type(&events, "shell");
    assert_eq!(
        foreground.len(),
        1,
        "only the second command ran to an exit"
    );
    let detached_at = detached[0]["timestamp"].as_u64().unwrap();
    let second_at = foreground[0]["timestamp"].as_u64().unwrap();
    assert!(
        second_at >= detached_at && second_at - detached_at < 4_000,
        "the second command ran {}ms after the demotion, outside the first command's window",
        second_at.saturating_sub(detached_at)
    );
}

/// A session ending with a demoted command still running exits without waiting for it, and says
/// so — once in the trace and once on stderr.
#[test]
fn shell_records_work_abandoned_when_the_session_ends() {
    if common::skip_without_host_support("shell_records_work_abandoned_when_the_session_ends") {
        return;
    }
    let server = ScriptedServer::start(vec![
        bash_call("msg_1", "toolu_forever", "sleep 30; echo never-seen"),
        end_turn("msg_2", "Started it."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    // `after_task: exit` is the default; only the grace period is declared.
    let manifest_path = create_agent_project_with_lifecycle(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash", "sleep"],
        "lifecycle:\n  shell_grace_secs: 1\n",
    );

    // Run through the binary rather than in-process: the abandonment is announced on the
    // process's own stderr, which is only observable from outside it. Redirected to files
    // rather than pipes, so what is timed is the process exiting rather than the last descriptor
    // its subprocess tree holds being closed.
    let stdout_path = project.path().join("mur-stdout.txt");
    let stderr_path = project.path().join("mur-stderr.txt");
    let started = std::time::Instant::now();
    let status = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            "Start a long build.",
            "--json",
        ])
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .status()
        .expect("mur run should execute");
    let elapsed = started.elapsed();
    let stderr = fs::read_to_string(&stderr_path).unwrap();
    assert!(status.success(), "mur run failed:\n{stderr}");
    // A generous bound only: most of this is staging and compiling the driver component, which
    // this test says nothing about. The claim that the session did not wait for the command is
    // made against the trace's own clock below.
    assert!(
        elapsed < std::time::Duration::from_secs(120),
        "mur run took {elapsed:?}"
    );

    let stdout = fs::read_to_string(&stdout_path).unwrap();
    let workdir = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|line| line["workdir"].as_str().map(std::path::PathBuf::from))
        .unwrap_or_else(|| panic!("no JSON launch line on stdout:\n{stdout}"));

    let events = trace_events(&workdir);
    let detached = events_of_type(&events, "shell_detached");
    assert_eq!(detached.len(), 1, "exactly one command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap();

    let abandoned = events_of_type(&events, "shell_abandoned");
    assert_eq!(abandoned.len(), 1, "exactly one abandonment is recorded");
    assert_eq!(abandoned[0]["work_id"], work_id);
    assert!(
        events_of_type(&events, "shell_completed").is_empty(),
        "an abandoned command produces no completion"
    );

    assert!(
        stderr.contains(work_id),
        "stderr must name the abandoned work id {work_id}:\n{stderr}"
    );

    let session_end = events_of_type(&events, "session_end");
    assert_eq!(session_end.len(), 1, "the session still ends normally");
    assert_eq!(
        session_end[0]["exit_status"], "ok",
        "abandonment does not change the session's exit status"
    );

    let detached_at = detached[0]["timestamp"].as_u64().unwrap();
    let ended_at = session_end[0]["timestamp"].as_u64().unwrap();
    assert!(
        ended_at - detached_at < 5_000,
        "the session ended {}ms after the demotion, so it waited on a 30-second command",
        ended_at - detached_at
    );
}
