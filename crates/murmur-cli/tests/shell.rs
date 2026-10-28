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

const DRIVER_ANTHROPIC_NAME: &str = "local/murmur-driver-anthropic";
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
    fs::write(
        staged.accessible_workdir.join("task.md"),
        "Run ls and summarize it.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let output = fs::read_to_string(launched.records_dir.join("out/result.txt")).unwrap();
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
    fs::write(staged.accessible_workdir.join("task.md"), "Try curl").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should not trap");
    let output = fs::read_to_string(launched.records_dir.join("out/result.txt")).unwrap();
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
        staged.accessible_workdir.join("task.md"),
        "Run a failing bash command.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    assert!(launched.records_dir.join("out/result.txt").exists());

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
    let shell_allow_yaml = shell_allow
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: agent-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver_name}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n{shell_allow_yaml}inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {driver_name}\n{lifecycle_yaml}"
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

fn trace_events(records_dir: &Path) -> Vec<Value> {
    fs::read_to_string(records_dir.join("trace.jsonl"))
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
        staged.accessible_workdir.join("task.md"),
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
    // The text names the tool that takes the work id. The default lifecycle delivers no
    // completion task, so it does not mention one.
    assert!(
        demotion.contains("background-command") && !demotion.contains("separate task"),
        "the result names the tool and no later task:\n{demotion}"
    );

    let events = trace_events(&launched.records_dir);
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
    let records_dir = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|line| line["records_dir"].as_str().map(std::path::PathBuf::from))
        .unwrap_or_else(|| panic!("no JSON launch line on stdout:\n{stdout}"));

    let events = trace_events(&records_dir);
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
    let log = records_dir.join("logs").join(format!("{work_id}.log"));
    assert!(
        stderr.contains(&format!("before the kill is in {}.", log.display())),
        "the report names the log by its absolute path in the records directory:\n{stderr}"
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

/// One scripted `tool_use` response calling `name` with `input`.
fn tool_call(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// The first `wrk_<hex>` id in `text`.
fn work_id_in(text: &str) -> Option<String> {
    let start = text.find("wrk_")?;
    let hex: String = text[start + 4..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        .collect();
    (!hex.is_empty()).then(|| format!("wrk_{hex}"))
}

/// Whether `pid` has exited within `timeout`: gone, or a zombie nobody has reaped yet.
fn process_gone_within(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !common::formation::alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Kills the pid written in each file that still names a live process when dropped, so a
/// failing test leaves nothing running.
struct KillPidFilesOnDrop(Vec<PathBuf>);

impl Drop for KillPidFilesOnDrop {
    fn drop(&mut self) {
        for file in &self.0 {
            if let Some(pid) = fs::read_to_string(file)
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok())
                .filter(|pid| *pid > 1)
            {
                common::signal(pid, libc::SIGKILL);
            }
        }
    }
}

/// The agent reads a demoted command's progress and then its exit through the work id, so its
/// result arrives inside the task that started it and no completion task follows.
#[test]
fn a_demoted_command_is_read_back_through_its_handle() {
    if common::skip_without_host_support("a_demoted_command_is_read_back_through_its_handle") {
        return;
    }
    let server = ScriptedServer::start_answering(4, {
        let mut turn = 0;
        let mut work_id = String::new();
        move |request: &Value| {
            turn += 1;
            match turn {
                1 => bash_call(
                    "msg_1",
                    "toolu_ticks",
                    "for i in 1 2 3; do echo tick $i; sleep 1; done; echo 'RESULT 42 passed'; \
                     exit 0",
                ),
                2 => {
                    let demotion = find_tool_result_block(request, "toolu_ticks")
                        .map(tool_result_text)
                        .unwrap_or_default();
                    work_id = work_id_in(&demotion).unwrap_or_default();
                    tool_call(
                        "msg_2",
                        "toolu_status",
                        "background-command",
                        json!({"operation": "status", "work_id": work_id}),
                    )
                }
                3 => tool_call(
                    "msg_3",
                    "toolu_wait",
                    "background-command",
                    json!({"operation": "wait", "work_id": work_id, "wait_secs": 30}),
                ),
                _ => end_turn("msg_4", "RESULT 42 passed, exit 0."),
            }
        }
    });

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
        staged.accessible_workdir.join("task.md"),
        "Run the suite and report.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "four turns were scripted");

    let status = tool_result_text(
        find_tool_result_block(&requests[2], "toolu_status").expect("the status call's result"),
    );
    assert!(
        status.contains("state: running") || status.contains("state: exited"),
        "{status}"
    );
    assert!(status.contains("tick 1"), "{status}");
    let waited = tool_result_text(
        find_tool_result_block(&requests[3], "toolu_wait").expect("the wait call's result"),
    );
    for expected in [
        "state: exited",
        "exit_code: 0",
        "status: ok",
        "RESULT 42 passed",
    ] {
        assert!(waited.contains(expected), "{expected}: {waited}");
    }

    let events = trace_events(&launched.records_dir);
    let detached = events_of_type(&events, "shell_detached");
    assert_eq!(detached.len(), 1, "exactly one command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap();
    let operations = events_of_type(&events, "background_command");
    assert_eq!(operations.len(), 2, "{operations:?}");
    assert_eq!(operations[0]["operation"], "status");
    assert_eq!(operations[0]["work_id"], work_id);
    assert_eq!(operations[1]["operation"], "wait");
    assert_eq!(operations[1]["state"], "exited");
    assert_eq!(operations[1]["exit_code"], 0);
    let completed = events_of_type(&events, "shell_completed");
    assert_eq!(completed.len(), 1, "{completed:?}");
    assert_eq!(completed[0]["work_id"], work_id);
    assert!(
        completed[0].get("completion_task_id").is_none(),
        "no completion task was enqueued: {}",
        completed[0]
    );
    assert!(events_of_type(&events, "shell_abandoned").is_empty());
    let task_ends = events_of_type(&events, "task_end");
    assert_eq!(task_ends.len(), 1, "no completion task ran: {task_ends:?}");
    assert!(
        task_ends[0].get("running_commands").is_none(),
        "{}",
        task_ends[0]
    );
    assert!(
        events_of_type(&events, "task_start")
            .iter()
            .all(|start| start["source"] != "detached_shell"),
        "no detached_shell task started"
    );
}

/// A task that ends with a demoted command still running names it on its `task_end` record, and
/// the session's end kills the command and the process it started in its group.
#[test]
fn a_task_that_ends_with_a_command_running_says_so_and_the_session_kills_it() {
    const TEST: &str = "a_task_that_ends_with_a_command_running_says_so_and_the_session_kills_it";
    if common::skip_without_host_support(TEST) {
        return;
    }
    let server = ScriptedServer::start(vec![
        bash_call(
            "msg_1",
            "toolu_server",
            "echo $$ > leader.pid; sleep 300 > /dev/null 2>&1 & echo $! > child.pid; sleep 300",
        ),
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
    let manifest_path = create_agent_project_with_lifecycle(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash", "sleep"],
        "lifecycle:\n  shell_grace_secs: 1\n",
    );
    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.accessible_workdir.clone();
    let _guard = KillPidFilesOnDrop(vec![workdir.join("leader.pid"), workdir.join("child.pid")]);
    fs::write(workdir.join("task.md"), "Start the server.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    let events = trace_events(&launched.records_dir);
    let detached = events_of_type(&events, "shell_detached");
    assert_eq!(detached.len(), 1, "exactly one command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap();
    let task_ends = events_of_type(&events, "task_end");
    assert_eq!(task_ends.len(), 1);
    assert_eq!(task_ends[0]["running_commands"], json!([work_id]));
    let abandoned = events_of_type(&events, "shell_abandoned");
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0]["work_id"], work_id);

    for file in ["leader.pid", "child.pid"] {
        let pid: u32 = fs::read_to_string(workdir.join(file))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            process_gone_within(pid, std::time::Duration::from_secs(2)),
            "{file}'s process {pid} outlived its session"
        );
    }
}

/// The records directories of `project_dir`'s sessions under `home`, newest last. The launch
/// writes one work folder per run under `<manifest dir>/workdir`, which is where `--resume`
/// looks for the one it names, and one records directory per run in that directory's records
/// group, named for the same session.
fn session_records(home: &Path, project_dir: &Path) -> Vec<PathBuf> {
    let group = common::records_group(home, &project_dir.join("workdir"));
    let mut dirs: Vec<PathBuf> = fs::read_dir(group)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_dir()
                        && path
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with("ses_"))
                })
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn session_name(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// `mur run` as a child process, with its output redirected to files under `project_dir` so a
/// process that is killed still leaves what it printed behind.
fn spawn_mur_run(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
    args: &[&str],
    tag: &str,
) -> std::process::Child {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .env("HOME", home.path())
        .arg("run")
        .arg("--manifest")
        .arg(manifest_path)
        .args(args)
        .stdout(std::fs::File::create(project_dir.join(format!("{tag}-stdout.txt"))).unwrap())
        .stderr(std::fs::File::create(project_dir.join(format!("{tag}-stderr.txt"))).unwrap());
    command.spawn().expect("mur run should execute")
}

/// Block until `predicate` holds over the newest session's trace, or fail; returns that session's
/// records directory.
fn await_trace<F: Fn(&[Value]) -> bool>(
    home: &Path,
    project_dir: &Path,
    what: &str,
    predicate: F,
) -> PathBuf {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        if let Some(dir) = session_records(home, project_dir).last() {
            if predicate(&trace_events(dir)) {
                return dir.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A demoted command whose runtime is killed outright is reported by the next resume, as a loss
/// and never as a result — and reported once, because the marker that reports it also clears it.
#[test]
fn shell_work_lost_to_a_killed_runtime_is_reported_once_on_resume() {
    if common::skip_without_host_support(
        "shell_work_lost_to_a_killed_runtime_is_reported_once_on_resume",
    ) {
        return;
    }
    let server = ScriptedServer::start(vec![
        bash_call("msg_1", "toolu_build", "sleep 60; echo never-seen"),
        end_turn("msg_2", "Started the build."),
        end_turn("msg_3", "Continued once."),
        end_turn("msg_4", "Noted the lost work."),
        end_turn("msg_5", "Continued twice."),
        end_turn("msg_6", "Nothing outstanding."),
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

    // `after_task: sleep` holds the first run open after its task, so the kill lands while the
    // demoted command is still outstanding rather than racing the session's own exit.
    let killed_manifest = create_agent_project_with_lifecycle(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash", "sleep"],
        "lifecycle:\n  shell_grace_secs: 1\n  task_acceptance: queue\n  after_task: sleep\n",
    );

    let mut killed = spawn_mur_run(
        &home,
        project.path(),
        &killed_manifest,
        &["--task", "Start a long build.", "--json"],
        "killed",
    );
    // Both, not just the demotion: the task has to be over for the run to have reached the wait
    // the kill interrupts.
    let killed_dir = await_trace(
        home.path(),
        project.path(),
        "the first run to demote and finish its task",
        |events| {
            !events_of_type(events, "shell_detached").is_empty()
                && !events_of_type(events, "task_end").is_empty()
        },
    );
    killed.kill().expect("SIGKILL reaches the run");
    killed.wait().expect("the killed run is reaped");

    // The teardown sweep never ran, so the operator surface it writes to holds nothing about this
    // command. Reconciliation on the next resume is the whole report for a killed runtime.
    let killed_bootstrap =
        fs::read_to_string(killed_dir.join("logs").join("bootstrap.log")).unwrap_or_default();
    assert!(
        !killed_bootstrap.contains("discarded at session end"),
        "a killed runtime writes no abandonment report:\n{killed_bootstrap}"
    );

    let killed_events = trace_events(&killed_dir);
    let detached = events_of_type(&killed_events, "shell_detached");
    assert_eq!(detached.len(), 1, "exactly one command was demoted");
    let work_id = detached[0]["work_id"].as_str().unwrap().to_string();
    let detached_at_ms = detached[0]["timestamp"].as_u64().unwrap();
    let binary = detached[0]["binary"].as_str().unwrap().to_string();
    assert!(
        events_of_type(&killed_events, "session_end").is_empty(),
        "a killed runtime writes no session_end, which is what leaves the demotion unaccounted"
    );

    // ── First resume: the loss is reported ──
    // No `lifecycle:` block at all, so the resume runs on the defaults — `task_acceptance:
    // single`, one task and out. The report is not an incoming task, so it still runs.
    let resume_manifest = create_agent_project(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &["bash", "sleep"],
    );
    let killed_session = session_name(&killed_dir);
    let status = spawn_mur_run(
        &home,
        project.path(),
        &resume_manifest,
        &["--resume", &killed_session, "--task", "Carry on.", "--json"],
        "resume-1",
    )
    .wait()
    .unwrap();
    let resume_stderr = fs::read_to_string(project.path().join("resume-1-stderr.txt")).unwrap();
    assert!(status.success(), "the resume failed:\n{resume_stderr}");

    let resumed_dir = session_records(home.path(), project.path())
        .into_iter()
        .find(|dir| dir != &killed_dir)
        .expect("the resume opened its own session");
    let resumed_events = trace_events(&resumed_dir);
    let resumed_session = session_name(&resumed_dir);
    let resumed_workdir = project.path().join("workdir").join(&resumed_session);
    let reports: Vec<&Value> = events_of_type(&resumed_events, "task_start")
        .into_iter()
        .filter(|event| event["source"] == "detached_lost")
        .collect();
    assert_eq!(reports.len(), 1, "one task reports the whole loss");
    assert_eq!(reports[0]["origin"], "completion");
    assert_eq!(reports[0]["lane"], "bg");

    // The text the agent was handed for the report: the user message of the request that
    // carries it.
    let message = server
        .requests()
        .iter()
        .map(common::last_user_text)
        .find(|text| text.contains(work_id.as_str()))
        .expect("a request carries the loss report");
    assert!(!resumed_workdir.join("task.md").exists());
    for named in [
        format!("work_id: {work_id}"),
        format!("detached_at_ms: {detached_at_ms}"),
    ] {
        assert!(
            message.lines().any(|line| line == named),
            "the report must name {named}:\n{message}"
        );
    }
    // The work id names the command; the binary and the command text stay in the trace.
    for absent in [
        "binary:",
        "command:",
        binary.as_str(),
        "sleep 60; echo never-seen",
    ] {
        assert!(
            !message.contains(absent),
            "the report must not repeat {absent}:\n{message}"
        );
    }
    assert!(
        !message.starts_with("Background shell command finished."),
        "the report must not open like a completion:\n{message}"
    );
    assert!(
        !message.contains(&format!("{work_id}.log")),
        "the report names no log path:\n{message}"
    );

    let killed_events = trace_events(&killed_dir);
    let lost = events_of_type(&killed_events, "shell_lost");
    assert_eq!(lost.len(), 1, "one marker accounts for the demotion");
    assert_eq!(lost[0]["work_id"], work_id.as_str());
    assert_eq!(lost[0]["session_id"], killed_session.as_str());
    assert_eq!(lost[0]["reconciled_by_session"], resumed_session.as_str());
    assert_eq!(lost[0]["detached_at_ms"], detached_at_ms);
    for absent in ["exit_code", "status", "duration_ms", "output_path"] {
        assert!(
            lost[0].get(absent).is_none(),
            "a lost command has no {absent}: {}",
            lost[0]
        );
    }
    for events in [&killed_events, &resumed_events] {
        for event_type in ["shell_completed", "shell_abandoned"] {
            assert!(
                events_of_type(events, event_type)
                    .iter()
                    .all(|event| event["work_id"] != work_id.as_str()),
                "{work_id} must never be reported as a {event_type}"
            );
        }
    }
    // The log is written as the command runs, so the killed session's holds a header and no
    // exit code; the resumed session writes none.
    let killed_log = fs::read_to_string(killed_dir.join("logs").join(format!("{work_id}.log")))
        .expect("the demoted command's log is written as it runs");
    assert!(killed_log.starts_with("Command: "), "{killed_log}");
    assert!(
        !killed_log.contains("Exit code:"),
        "a killed runtime records no exit code: {killed_log}"
    );
    assert!(!resumed_dir
        .join("logs")
        .join(format!("{work_id}.log"))
        .exists());
    assert!(!resumed_workdir.join("logs").exists());

    // ── Second resume: the marker has cleared it ──
    let status = spawn_mur_run(
        &home,
        project.path(),
        &resume_manifest,
        &[
            "--resume",
            &killed_session,
            "--task",
            "Carry on again.",
            "--json",
        ],
        "resume-2",
    )
    .wait()
    .unwrap();
    let second_stderr = fs::read_to_string(project.path().join("resume-2-stderr.txt")).unwrap();
    assert!(
        status.success(),
        "the second resume failed:\n{second_stderr}"
    );

    let second_dir = session_records(home.path(), project.path())
        .into_iter()
        .find(|dir| dir != &killed_dir && dir != &resumed_dir)
        .expect("the second resume opened its own session");
    assert!(
        events_of_type(&trace_events(&second_dir), "task_start")
            .iter()
            .all(|event| event["source"] != "detached_lost"),
        "a marked work id is not reported again"
    );
    assert_eq!(
        events_of_type(&trace_events(&killed_dir), "shell_lost").len(),
        1,
        "a second resume adds no second marker"
    );
}

/// A prior trace that cannot be read, and one whose writer was killed mid-line, both leave the
/// resume running: reconciliation reports nothing rather than refusing the launch.
#[test]
fn a_resume_over_an_unreadable_prior_trace_still_runs_its_task() {
    if common::skip_without_host_support(
        "a_resume_over_an_unreadable_prior_trace_still_runs_its_task",
    ) {
        return;
    }
    let server = ScriptedServer::start(vec![
        end_turn("msg_1", "First run."),
        end_turn("msg_2", "Resumed over a torn trace."),
        end_turn("msg_3", "Noted the record above the tear."),
        end_turn("msg_4", "Resumed over an unreadable trace."),
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
    fs::write(staged.accessible_workdir.join("task.md"), "Say something.").unwrap();
    let first = launch_session(staged, |_| {}).expect("the first launch should succeed");
    let first_session = first.session_id.clone();
    let context_id = trace_events(&first.records_dir)
        .iter()
        .find(|event| event["event_type"] == "task_start")
        .and_then(|event| event["context_id"].as_str().map(str::to_string))
        .expect("the first run recorded a context id");

    // A trace whose writer was killed mid-record: a complete `shell_detached` line followed by a
    // partial one.
    let trace_path = first.records_dir.join("trace.jsonl");
    let mut torn = fs::read_to_string(&trace_path).unwrap();
    torn.push_str(
        "{\"event_type\":\"shell_detached\",\"event_id\":\"evt_torn\",\"session_id\":\"s\",\"timestamp\":1750,\"turn\":1,\"task_id\":\"tsk_x\",\"work_id\":\"wrk_torn\",\"binary\":\"/usr/bin/bash\",\"command\":\"sleep 60\",\"grace_ms\":1000}\n{\"event_type\":\"shell_com",
    );
    fs::write(&trace_path, &torn).unwrap();

    let staged = common::stage_agent_session_resuming(
        &home,
        project.path(),
        &manifest_path,
        &first_session,
        &context_id,
    );
    fs::write(staged.accessible_workdir.join("task.md"), "Continue.").unwrap();
    let resumed =
        launch_session(staged, |_| {}).expect("a torn prior trace must not fail a resume");
    assert!(
        resumed.records_dir.join("out/result.txt").exists(),
        "the resumed session still ran its task"
    );
    // Read leniently: this file ends mid-record, which is the shape the scan has to tolerate.
    let torn_events: Vec<Value> = fs::read_to_string(&trace_path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(
        events_of_type(&torn_events, "shell_lost").len(),
        1,
        "the complete record above the tear is still accounted for"
    );
    // This manifest declares no `lifecycle:` block, so the resume runs on the defaults. The
    // report still reaches the agent: a marker is only written for a loss that gets reported.
    assert_eq!(
        events_of_type(&trace_events(&resumed.records_dir), "task_start")
            .iter()
            .filter(|event| event["source"] == "detached_lost")
            .count(),
        1,
        "a default capsule reports the loss as well as marking it"
    );

    // Unreadable rather than unparseable: the read itself fails, and the launch still runs.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&trace_path, fs::Permissions::from_mode(0o000)).unwrap();
    let staged = common::stage_agent_session_resuming(
        &home,
        project.path(),
        &manifest_path,
        &first_session,
        &context_id,
    );
    fs::write(staged.accessible_workdir.join("task.md"), "Continue again.").unwrap();
    let resumed =
        launch_session(staged, |_| {}).expect("an unreadable prior trace must not fail a resume");
    assert!(
        resumed.records_dir.join("out/result.txt").exists(),
        "the resumed session still ran its task"
    );
    fs::set_permissions(&trace_path, fs::Permissions::from_mode(0o644)).unwrap();
}
