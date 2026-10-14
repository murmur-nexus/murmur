//! A session's shell calls share one network namespace: a server one call starts answers the
//! next call, and nothing the session started outlives it.
//!
//! The model is scripted. What runs is real: the staged session, the kernel-tier sandbox, the
//! session's namespace and its cgroup scope.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use capsule_runtime::launch_session;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

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

/// A capsule that may run `bash`, `nc` and `sleep`, with the default lifecycle.
fn create_manifest(project_dir: &Path, endpoint: &str) -> std::path::PathBuf {
    let manifest = format!(
        concat!(
            "name: netns-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "    gateway:\n",
            "      endpoint: {endpoint}\n",
            "      api_key: test-key\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "      - nc\n",
            "      - sleep\n",
            "inference:\n",
            "  transport: http\n",
            "  model: test-model\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// Turn 1 backgrounds a one-shot server and a long sleeper; turn 2, a separate `bash` call,
/// reads the server's reply and checks the sleeper is alive. After the session ends, the sleeper
/// is gone.
#[test]
#[allow(clippy::print_stdout)]
fn a_server_from_one_shell_call_answers_the_next() {
    if common::skip_without_host_support("a_server_from_one_shell_call_answers_the_next") {
        return;
    }
    let start_server = "(printf \"pong\\n\" | nc -N -l 127.0.0.1 8080) >/dev/null 2>&1 & \
                        echo $! > server.pid; \
                        sleep 300 >/dev/null 2>&1 & echo $! > sleeper.pid";
    let reach_server = "for i in {1..50}; do exec 3<>/dev/tcp/127.0.0.1/8080 2>/dev/null && \
                        break; sleep 0.1; done; read -r reply <&3; echo \"$reply\"; \
                        kill -0 \"$(<sleeper.pid)\" && echo sleeper-alive";

    let server = common::ScriptedServer::start(vec![
        tool_call(
            "msg_1",
            "toolu_server",
            "bash",
            json!({"command": start_server}),
        ),
        tool_call(
            "msg_2",
            "toolu_client",
            "bash",
            json!({"command": reach_server}),
        ),
        end_turn("msg_3", "The server answered."),
    ]);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    let accessible = staged.accessible_workdir.clone();
    fs::write(accessible.join("task.md"), "Start a server, then reach it.").unwrap();

    launch_session(staged, |_| {}).expect("the session launches");

    let requests = server.requests();
    let reply = common::tool_result_text(&requests, "toolu_client")
        .expect("the second bash call returned a tool_result");
    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
    let shells: Vec<Value> = trace_raw
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event_type"] == "shell")
        .collect();
    let sleeper: i32 = fs::read_to_string(accessible.join("sleeper.pid"))
        .expect("the first call wrote sleeper.pid")
        .trim()
        .parse()
        .unwrap();
    // The sleeper is the runtime's grandchild, orphaned to `init` when its shell exited; its
    // `/proc` entry goes once `init` reaps it, a moment after the kill.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while Path::new(&format!("/proc/{sleeper}")).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let sleeper_alive_after = Path::new(&format!("/proc/{sleeper}")).exists();

    println!(
        "turn-2 tool_result: {reply:?}; shell records: {:?}; /proc/{sleeper} after the session: \
         {}",
        shells
            .iter()
            .map(|event| &event["exit_code"])
            .collect::<Vec<_>>(),
        if sleeper_alive_after {
            "exists"
        } else {
            "absent"
        }
    );
    assert!(reply.contains("pong"), "tool_result: {reply}");
    assert!(reply.contains("sleeper-alive"), "tool_result: {reply}");
    assert_eq!(shells.len(), 2, "trace was:\n{trace_raw}");
    for shell in &shells {
        assert_eq!(shell["exit_code"], 0, "trace was:\n{trace_raw}");
    }
    assert!(
        !sleeper_alive_after,
        "a process the session backgrounded outlived the session"
    );
}
