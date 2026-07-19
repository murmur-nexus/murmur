#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path, path::PathBuf};

use capsule_runtime::launch_session;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
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
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

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
            format!("  - name: {name}\n    version: {version}\n    runtime: {runtime}\n")
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
            "{extra_artifact_entries}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "{shell_section}",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: claude-test-model\n",
            "  api_key: test-key\n",
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

/// Test 1: MURMUR.md is written to the workdir root after stage_session.
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

    let murmur_md_path = staged.workdir.join("MURMUR.md");
    assert!(
        murmur_md_path.exists(),
        "MURMUR.md should exist in workdir after staging"
    );

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
        content.contains("## Directory Layout"),
        "MURMUR.md should include the directory layout section"
    );
    assert!(
        content.contains("## Installed Tools"),
        "MURMUR.md should include the installed tools section"
    );
    assert!(
        content.contains("## Managing Artifacts at Runtime"),
        "MURMUR.md should include the managing artifacts section"
    );
}

/// Test 2: MURMUR.md reflects model name, context budget, and shell capabilities.
#[test]
fn murmur_md_reflects_capsule_config() {
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
    let content = fs::read_to_string(staged.workdir.join("MURMUR.md")).unwrap();

    assert!(
        content.contains("Model: claude-test-model"),
        "MURMUR.md should include the model name; content:\n{content}"
    );
    assert!(
        content.contains("100000 tokens"),
        "MURMUR.md should include context.max_tokens"
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
    let content = fs::read_to_string(staged.workdir.join("MURMUR.md")).unwrap();

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

/// Test 4: native artifact binary is staged to workdir/tools/<name>/<name> and is executable.
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
        .workdir
        .join("tools")
        .join("test-native")
        .join("test-native");
    assert!(
        binary_path.exists(),
        "native binary should exist at workdir/tools/test-native/test-native"
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

    // Script creates a marker file so we can assert it actually ran.
    let native_script = "#!/bin/sh\nmkdir -p native-ran\ntouch native-ran/marker.txt\necho '{\"status\":\"passed\",\"summary\":\"native ran ok\",\"data\":\"{}\",\"data_path\":null,\"truncated\":false,\"metadata\":[]}'\n";
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
    fs::write(staged.workdir.join("task.md"), "Use test-native.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("launch should succeed");

    assert!(
        launched
            .workdir
            .join("native-ran")
            .join("marker.txt")
            .exists(),
        "native tool should have created the marker file"
    );
    assert!(
        launched.workdir.join("out").join("result.txt").exists(),
        "out/result.txt should exist after agent completes"
    );
}

/// Test 6: murmur-scaffold dispatch creates expected tool directory structure.
///
/// Requires murmur-scaffold@0.1.0 in the local registry (~/.murmur/artifacts/).
/// Skips gracefully if not present.
#[cfg(unix)]
#[test]
fn scaffold_creates_tool_directory() {
    let scaffold_zip_path = std::env::var("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join(".murmur/artifacts/murmur-scaffold/0.1.0/murmur-scaffold-0.1.0.mur.zip")
        })
        .unwrap_or_default();

    if !scaffold_zip_path.exists() {
        eprintln!(
            "skipping scaffold_creates_tool_directory: murmur-scaffold@0.1.0 not in local registry"
        );
        return;
    }

    let server = ScriptedServer::start(two_turn_responses_with_tool("murmur-scaffold"));

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();
    common::publish_local(&home, &scaffold_zip_path).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        DRIVER_ANTHROPIC_NAME,
        &[("murmur-scaffold", "0.1.0", "tool")],
        &[],
        None,
    );

    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Scaffold a new tool.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("launch should succeed");

    // The tool call in two_turn_responses_with_tool sends name=new-tool
    let new_tool_dir = launched.workdir.join("tools").join("new-tool");
    assert!(
        new_tool_dir.exists(),
        "scaffold should have created tools/new-tool/"
    );
    assert!(
        new_tool_dir.join("murmur.yaml").exists(),
        "scaffold should have created murmur.yaml"
    );
    assert!(
        new_tool_dir.join("bin").join("run").exists(),
        "scaffold should have created bin/run"
    );
    assert!(
        new_tool_dir.join("README.md").exists(),
        "scaffold should have created README.md"
    );
}

/// Test 7: skill artifact installs skill.md to tools/<name>/skill.md.
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
        .workdir
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

/// Test 8: MURMUR.md includes a skills section listing installed skill artifacts.
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
    let content = fs::read_to_string(staged.workdir.join("MURMUR.md")).unwrap();

    assert!(
        content.contains("## Installed Skills"),
        "MURMUR.md should include a skills section; content:\n{content}"
    );
    assert!(
        content.contains("my-guide-skill"),
        "MURMUR.md skills section should name the installed skill"
    );
    assert!(
        content.contains("*(call by name to load guidance)*"),
        "MURMUR.md skills section should mark the skill as callable by name; content:\n{content}"
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
