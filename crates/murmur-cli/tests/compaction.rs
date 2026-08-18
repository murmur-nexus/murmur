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

/// Stage the session from a manifest that includes context and compaction fields.
fn stage_compaction_session(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

/// Write a minimal murmur-compact tool directory (no WASM — triggers native fallback).
fn stage_compact_tool_dir(workdir: &Path) {
    let compact_dir = workdir.join("tools").join("murmur-compact");
    fs::create_dir_all(&compact_dir).unwrap();
    fs::write(
        compact_dir.join("murmur.yaml"),
        "name: murmur-compact\nversion: 0.1.0\nruntime: driver\ndescription: Compact conversation history\n",
    )
    .unwrap();
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
                "id": "toolu_ls",
                "name": "bash",
                "input": {"command": "echo hello"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 50, "output_tokens": 50}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "Task complete."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 50, "output_tokens": 50}
        })
        .to_string(),
    ]
}

/// Test 1: compaction fires when threshold is reached and a compaction hook is present.
/// Skipped: requires a hook WASM artifact that implements `on-compaction` and returns
/// `replace-context`. The old `native_compact` / `murmur-compact` tool path has been removed.
/// Re-enable once a compaction hook fixture is available in the test suite.
#[test]
#[ignore = "needs a compaction hook WASM fixture; native_compact path removed"]
fn compaction_fires_when_threshold_reached() {
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    // Manifest with very small context_window and very low threshold so compaction
    // fires on the first turn.
    let manifest_path = create_compaction_manifest(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        true, // include context.max_tokens
        true, // include shell.allow: [bash]
    );

    let staged = stage_compaction_session(&home, project.path(), &manifest_path);

    // Manually create murmur-compact tool dir (no WASM → triggers native fallback).
    stage_compact_tool_dir(&staged.workdir);

    fs::write(staged.workdir.join("task.md"), "Run echo and summarize.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    // out/result.txt must exist
    assert!(
        launched.workdir.join("out/result.txt").exists(),
        "out/result.txt should exist"
    );

    // checkpoints/summary.md must exist with non-empty content
    let summary_path = launched.workdir.join("checkpoints/summary.md");
    assert!(
        summary_path.exists(),
        "checkpoints/summary.md should exist after compaction"
    );
    let summary = fs::read_to_string(&summary_path).unwrap_or_default();
    assert!(
        !summary.trim().is_empty(),
        "checkpoints/summary.md should contain non-empty content"
    );

    // logs/bootstrap.log must contain the compaction log line
    let log = fs::read_to_string(launched.workdir.join("logs/bootstrap.log")).unwrap_or_default();
    assert!(
        log.contains("[compaction] compacted at turn"),
        "bootstrap.log should contain compaction log line, got: {log}"
    );
}

/// Test 2: threshold reached but murmur-compact not declared → warning logged, loop continues.
#[test]
fn compact_not_declared_threshold_reached_warning_logged() {
    if common::skip_without_cgroup_delegation(
        "compact_not_declared_threshold_reached_warning_logged",
    ) {
        return;
    }
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_compaction_manifest(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        true, // context.max_tokens set
        true, // shell.allow: [bash]
    );

    let staged = stage_compaction_session(&home, project.path(), &manifest_path);
    // Do NOT create murmur-compact tool dir → compact_tool_name = None

    fs::write(staged.workdir.join("task.md"), "Run echo and summarize.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    // out/result.txt must exist
    assert!(
        launched.workdir.join("out/result.txt").exists(),
        "out/result.txt should exist"
    );

    // summary.md must NOT exist (compaction was skipped)
    assert!(
        !launched.workdir.join("checkpoints/summary.md").exists(),
        "checkpoints/summary.md should NOT exist when murmur-compact is not installed"
    );

    // bootstrap.log must contain warning that no hook returned replace-context
    let log = fs::read_to_string(launched.workdir.join("logs/bootstrap.log")).unwrap_or_default();
    assert!(
        log.contains("no hook returned replace-context"),
        "bootstrap.log should contain compaction skip warning, got: {log}"
    );
}

/// Test 3: context.max_tokens absent → compaction disabled even if murmur-compact installed.
#[test]
fn context_max_tokens_absent_compaction_disabled() {
    if common::skip_without_cgroup_delegation("context_max_tokens_absent_compaction_disabled") {
        return;
    }
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    // Manifest WITHOUT context.max_tokens; compaction.threshold is set but irrelevant.
    let manifest_path = create_compaction_manifest(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        false, // NO context.max_tokens
        true,  // shell.allow: [bash]
    );

    let staged = stage_compaction_session(&home, project.path(), &manifest_path);

    fs::write(staged.workdir.join("task.md"), "Run echo and summarize.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    // out/result.txt must exist
    assert!(
        launched.workdir.join("out/result.txt").exists(),
        "out/result.txt should exist"
    );

    // summary.md must NOT exist (compaction was disabled because context_window == 0)
    assert!(
        !launched.workdir.join("checkpoints/summary.md").exists(),
        "checkpoints/summary.md should NOT exist when context.max_tokens is absent"
    );
}

/// Create a test project manifest for compaction tests.
/// When `with_context` is true, sets context.max_tokens=200 and compaction.threshold=0.01
/// so compaction fires on the very first turn.
fn create_compaction_manifest(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    with_context: bool,
    with_shell_bash: bool,
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let context_section = if with_context {
        format!("context:\n  max_tokens: 200\n")
    } else {
        String::new()
    };

    let shell_section = if with_shell_bash {
        "  shell:\n    allow:\n      - bash\n".to_string()
    } else {
        String::new()
    };

    // Build the manifest string with explicit indentation — never use `\` line
    // continuations in format! when indentation matters.
    let manifest = format!(
        concat!(
            "name: agent-capsule\n",
            "version: 0.1.0\n",
            "{context_section}",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "{shell_section}",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "  compaction:\n",
            "    threshold: 0.01\n",
        ),
        context_section = context_section,
        driver_name = driver_name,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        shell_section = shell_section,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();

    project_dir.join("murmur.yaml")
}
