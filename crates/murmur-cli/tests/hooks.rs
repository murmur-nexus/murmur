#[path = "common/mod.rs"]
mod common;

use std::{fs, path::PathBuf};

use capsule_runtime::launch_session;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.7";
const HOOK_NAME: &str = "murmur-hook-debug";
const HOOK_VERSION: &str = "0.3.0";

#[test]
#[ignore = "requires a default-artifacts checkout with murmur-hook-debug built; set MURMUR_DEFAULT_ARTIFACTS_DIR to point at it"]
fn hook_debug_writes_lifecycle_jsonl() {
    let Some(artifacts_dir) = common::default_artifacts_dir() else {
        eprintln!(
            "[SKIP] hook_debug_writes_lifecycle_jsonl: set MURMUR_DEFAULT_ARTIFACTS_DIR to a \
             default-artifacts checkout with murmur-hook-debug built"
        );
        return;
    };

    let server = common::ScriptedServer::start(two_turn_responses());

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

    let hook_wasm = artifacts_dir.join("hooks/murmur-hook-debug/murmur_hook_debug.wasm");
    if !hook_wasm.exists() {
        eprintln!(
            "[SKIP] hook_debug_writes_lifecycle_jsonl: murmur-hook-debug wasm must be built in \
             the default-artifacts checkout first (looked at {})",
            hook_wasm.display()
        );
        return;
    }
    let hook_artifact =
        common::create_hook_artifact(artifact_dir.path(), HOOK_NAME, HOOK_VERSION, &hook_wasm);
    common::publish_local(&home, &hook_artifact).success();

    let manifest_path = create_manifest(project.path(), &server.endpoint);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let lines = fs::read_to_string(workdir.join("hook-debug.jsonl")).unwrap();
    let events: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();

    assert_eq!(
        names,
        vec![
            "stage",
            "session-start",
            "inference",
            "tool-call",
            "shell",
            "inference",
            "session-end"
        ]
    );

    // Look events up by name, never by index. The lifecycle gains events over time
    // (`stage` arrived exactly this way), and a positional index silently starts
    // asserting against the wrong event when it does.
    let find = |name: &str| {
        events
            .iter()
            .find(|event| event["event"] == name)
            .unwrap_or_else(|| panic!("no {name} event in {names:?}"))
    };

    let session_id = find("session-start")["session_id"].as_str().unwrap();
    assert!(
        session_id.starts_with("ses_"),
        "session_id must start with ses_"
    );
    assert_eq!(
        session_id.len(),
        36,
        "session_id must be 36 chars (ses_ + 32 hex)"
    );
    assert_eq!(find("session-end")["exit_status"], "ok");
}

fn create_manifest(project_dir: &std::path::Path, endpoint: &str) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let manifest = format!(
        concat!(
            "name: hook-test\n",
            "version: 0.1.7\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "  - name: {hook_name}\n",
            "    version: {hook_version}\n",
            "    runtime: hook\n",
            // Hooks are default-deny: murmur-hook-debug writes hook-debug.jsonl to its
            // working directory, so the operator has to grant it that directory here.
            "    capabilities:\n",
            "      filesystem:\n",
            "        scope: .\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        hook_name = HOOK_NAME,
        hook_version = HOOK_VERSION,
        endpoint = endpoint,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

fn two_turn_responses() -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_bash",
                "name": "bash",
                "input": {"command": "echo hello"}
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
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]
}
