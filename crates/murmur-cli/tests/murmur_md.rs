#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path, path::PathBuf};

use capsule_runtime::launch_session;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

/// Build a murmur.yaml in the project dir and return its path.
fn create_agent_manifest(
    project_dir: &Path,
    endpoint: &str,
    driver_name: &str,
    extra_artifacts: &[(&str, &str, &str)], // (name, version, runtime)
    shell_allow: &[&str],
    context_max_tokens: Option<u32>,
) -> PathBuf {
    let context_section = context_max_tokens
        .map(|n| format!("context:\n  max_tokens: {n}\n"))
        .unwrap_or_default();

    let shell_section = if shell_allow.is_empty() {
        String::new()
    } else {
        let entries = shell_allow
            .iter()
            .map(|b| format!("      - {b}\n"))
            .collect::<String>();
        format!("  shell:\n    allow:\n{entries}")
    };

    let extra_artifact_entries = extra_artifacts
        .iter()
        .map(|(name, version, runtime)| {
            format!("  - name: local/{name}\n    version: {version}\n    runtime: {runtime}\n")
        })
        .collect::<String>();

    let manifest = format!(
        concat!(
            "name: agent-capsule\n",
            "version: 0.1.0\n",
            "{context_section}",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "    gateway:\n",
            "      endpoint: {endpoint}\n",
            "      api_key: test-key\n",
            "{extra_artifact_entries}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "{shell_section}",
            "inference:\n",
            "  transport: http\n",
            "  model: claude-test-model\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        context_section = context_section,
        driver_name = driver_name,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        extra_artifact_entries = extra_artifact_entries,
        shell_section = shell_section,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// Test 1: MURMUR.md is written to the runtime directory after stage_session, and nowhere in the
/// workdir.
#[test]
fn murmur_md_written_after_staging() {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);

    let murmur_md_path = staged.runtime_dir.join("MURMUR.md");
    assert!(
        murmur_md_path.exists(),
        "MURMUR.md should exist in the runtime directory after staging"
    );
    assert!(!staged.accessible_workdir.join("MURMUR.md").exists());

    let content = fs::read_to_string(&murmur_md_path).unwrap();
    assert!(
        content.starts_with("# MURMUR.md — Capsule Environment Reference"),
        "MURMUR.md should start with the canonical header"
    );
    assert!(
        content.contains("murmur-runtime"),
        "MURMUR.md should mention murmur-runtime"
    );
    assert!(
        content.contains("## Runtime Directory"),
        "MURMUR.md should include the runtime directory section"
    );
    assert!(
        content.contains("## Limits"),
        "MURMUR.md should include the limits section"
    );
    assert!(
        content.contains("## Installed Tools"),
        "MURMUR.md should include the installed tools section"
    );
}

/// Test 2: MURMUR.md reflects model name, context budget, and shell capabilities.
#[test]
fn murmur_md_reflects_capsule_config() {
    if capsule_runtime::skip_without_egress_namespace("murmur_md_reflects_capsule_config") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[],
        &["bash"],
        Some(100_000),
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let content = fs::read_to_string(staged.runtime_dir.join("MURMUR.md")).unwrap();

    assert!(
        content.contains("Model: claude-test-model"),
        "MURMUR.md should include the model name; content:\n{content}"
    );
    assert!(
        content.contains(
            "- Context budget: 91808 input tokens, the context window less the output cap \
             (context window 100000, output cap 8192, input ceiling none) (compaction not \
             configured)\n"
        ),
        "MURMUR.md should state the input budget cut from context.max_tokens; content:\n{content}"
    );
    assert!(
        content.contains("bash"),
        "MURMUR.md should list the shell binary"
    );
}

/// Test 3: MURMUR.md lists LLM-visible tools and omits driver artifacts.
#[test]
fn murmur_md_shows_visible_tools_not_drivers() {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let native_script = "#!/bin/sh\necho '{\"status\":\"passed\",\"summary\":\"ok\",\"data\":null,\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";
    let native_artifact = common::create_native_artifact(
        artifact_dir.path(),
        "my-native-tool",
        "1.0.0",
        native_script,
        Some("Processes data and returns results"),
        None,
    );
    common::publish_local(&home, &native_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[("my-native-tool", "1.0.0", "tool")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let content = fs::read_to_string(staged.runtime_dir.join("MURMUR.md")).unwrap();

    assert!(
        !content.contains(&format!("**{DRIVER_ANTHROPIC_NAME}**")),
        "driver artifact should not appear in MURMUR.md installed tools"
    );
    assert!(
        content.contains("**my-native-tool**"),
        "native tool should appear in MURMUR.md installed tools; content:\n{content}"
    );
    assert!(
        content.contains("Processes data and returns results"),
        "tool description should appear in MURMUR.md"
    );
}

/// Test 4: native artifact binary is staged to <runtime dir>/tools/<name>/<name> and is executable.
#[test]
fn native_artifact_binary_staged_to_workdir() {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let native_script = "#!/bin/sh\necho '{\"status\":\"passed\",\"summary\":\"hello\",\"data\":null,\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";
    let native_artifact = common::create_native_artifact(
        artifact_dir.path(),
        "test-native",
        "0.1.0",
        native_script,
        None,
        None,
    );
    common::publish_local(&home, &native_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[("test-native", "0.1.0", "tool")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);

    let binary_path = staged
        .runtime_dir
        .join("tools")
        .join("test-native")
        .join("test-native");
    assert!(
        binary_path.exists(),
        "native binary should exist at <runtime dir>/tools/test-native/test-native"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&binary_path).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "native binary should be executable (mode: {mode:o})"
        );
    }
}

/// Test 5: native tool dispatch — agent calls a native binary, it executes and returns a result.
#[cfg(unix)]
#[test]
fn native_tool_dispatch_executes_binary_and_returns_result() {
    if common::skip_without_host_support("native_tool_dispatch_executes_binary_and_returns_result")
    {
        return;
    }
    let server = ScriptedServer::start(two_turn_responses_with_tool("test-native"));

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    // Script creates a marker file so we can assert it actually ran. Shell builtins only: a native
    // tool runs in the capsule's sandbox, where `mkdir` or `touch` would need `shell.allow`.
    let native_script = "#!/bin/sh\n: > native-ran-marker.txt\necho '{\"status\":\"passed\",\"summary\":\"native ran ok\",\"data\":\"{}\",\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";
    let native_artifact = common::create_native_artifact(
        artifact_dir.path(),
        "test-native",
        "0.1.0",
        native_script,
        Some("A native test tool"),
        None,
    );
    common::publish_local(&home, &native_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &[("test-native", "0.1.0", "tool")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(
        staged.accessible_workdir.join("task.md"),
        "Use test-native.",
    )
    .unwrap();

    let launched = launch_session(staged, |_| {}).expect("launch should succeed");

    assert!(
        launched.workdir.join("native-ran-marker.txt").exists(),
        "native tool should have created the marker file"
    );
    assert!(
        launched.records_dir.join("out").join("result.txt").exists(),
        "out/result.txt should exist in the records directory after the agent completes"
    );
}

/// Test 6: skill artifact installs skill.md to tools/<name>/skill.md.
#[test]
fn skill_artifact_installed_to_tools_dir() {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let skill_content = "# My Test Skill\n\nThis skill provides guidance for testing.\n";
    let skill_artifact =
        common::create_skill_artifact(artifact_dir.path(), "my-test-skill", "0.1.0", skill_content);
    common::publish_local(&home, &skill_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[("my-test-skill", "0.1.0", "skill")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);

    let skill_path = staged
        .runtime_dir
        .join("tools")
        .join("my-test-skill")
        .join("skill.md");
    assert!(
        skill_path.exists(),
        "skill.md should exist at tools/my-test-skill/skill.md"
    );

    let installed = fs::read_to_string(&skill_path).unwrap();
    assert_eq!(
        installed, skill_content,
        "installed skill.md content should match original"
    );
}

/// Test 7: MURMUR.md includes a skills section listing installed skill artifacts.
#[test]
fn murmur_md_includes_skills_section_when_skill_installed() {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let skill_artifact = common::create_skill_artifact(
        artifact_dir.path(),
        "my-guide-skill",
        "1.0.0",
        "# Guide\nRead this for context.\n",
    );
    common::publish_local(&home, &skill_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        "http://localhost:9999",
        DRIVER_ANTHROPIC_NAME,
        &[("my-guide-skill", "1.0.0", "skill")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let content = fs::read_to_string(staged.runtime_dir.join("MURMUR.md")).unwrap();

    assert!(
        content.contains("## Installed Skills"),
        "MURMUR.md should include a skills section; content:\n{content}"
    );
    assert!(
        content.contains("my-guide-skill"),
        "MURMUR.md skills section should name the installed skill"
    );
    let skill_path = staged
        .runtime_dir
        .join("tools/my-guide-skill/skill.md")
        .display()
        .to_string();
    assert!(
        content.contains(&format!("*(call by name to load guidance; {skill_path})*")),
        "MURMUR.md skills section should mark the skill as callable by name and name its path \
         in the runtime directory; content:\n{content}"
    );
    assert!(
        content.contains("it is not pre-injected\ninto context"),
        "MURMUR.md skills section should note skill content is not pre-injected; content:\n{content}"
    );
    assert!(
        !content.contains("**my-guide-skill**\n  Input:"),
        "skill artifact must not appear in the Installed Tools list"
    );
}

fn two_turn_responses_with_tool(tool_name: &str) -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": tool_name,
                "input": {"type": "tool", "name": "new-tool", "runtime": "native"}
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
            "content": [{"type": "text", "text": "Done."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 50, "output_tokens": 50}
        })
        .to_string(),
    ]
}
