#[path = "common/mod.rs"]
mod common;

use std::{collections::HashSet, fs, path::PathBuf};

use capsule_runtime::{stage_session, ArtifactRequest, CapabilityPolicy, StageRequest};
use murmur_artifact::{
    ArtifactRuntime, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
};

const SHELL_DESC_DRIVER_NAME: &str = "murmur-driver-shell-desc";
const SHELL_DESC_DRIVER_VERSION: &str = "0.1.0";

/// Path to the debug-built driver binary inside the default-artifacts
/// checkout. Build it there first:
///   cargo build -p murmur-driver-shell-desc
fn shell_desc_binary() -> PathBuf {
    common::default_artifacts_dir().join("target/debug/murmur-driver-shell-desc")
}

#[test]
#[ignore = "requires a default-artifacts checkout with murmur-driver-shell-desc built; set MURMUR_DEFAULT_ARTIFACTS_DIR or clone it next to this repo"]
fn shell_desc_driver_writes_enriched_manifest_for_known_binary() {
    let binary_path = shell_desc_binary();
    assert!(
        binary_path.exists(),
        "murmur-driver-shell-desc must be built in the default-artifacts checkout \
         first (looked at {})",
        binary_path.display()
    );

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let shell_desc_artifact = common::create_shell_desc_driver_artifact(
        artifact_dir.path(),
        SHELL_DESC_DRIVER_NAME,
        SHELL_DESC_DRIVER_VERSION,
        &binary_path,
    );
    common::publish_local(&home, &shell_desc_artifact).success();

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    // Create a minimal workdir project dir (no murmur.yaml needed for direct stage_session)
    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project.path().to_path_buf(),
            capsule_name: "test-shell-desc".to_string(),
            capsule_version: "0.1.0".to_string(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![ArtifactRequest {
                name: SHELL_DESC_DRIVER_NAME.to_string(),
                version: SHELL_DESC_DRIVER_VERSION.to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: CapabilityPolicy {
                shell_allow: vec!["git".to_string(), "my-custom-tool".to_string()],
                ..Default::default()
            },
            // Fake inference config — needed so empty capsule_component_bytes is accepted
            inference: Some(InferenceConfig {
                transport: "http".to_string(),
                endpoint: Some("http://localhost:9999".to_string()),
                model: "test-model".to_string(),
                api_key: None,
                driver: Some(InferenceDriver {
                    artifact: "fake-driver".to_string(),
                    config: None,
                }),
                command: None,
                compaction: None,
                system_prompt: None,
                system_prompt_file: None,
                system_prompt_artifact: None,
                max_turns: 10,
                max_task_reopens: 1,
                max_tokens: None,
            }),
            context: None,
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
            job_id: None,
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .expect("stage_session should succeed");

    let workdir = staged.workdir;

    // Scenario 1: known binary gets enriched manifest
    let git_manifest_path = workdir.join("tools").join("git").join("murmur.yaml");
    assert!(
        git_manifest_path.exists(),
        "git manifest should exist after staging"
    );
    let git_manifest = fs::read_to_string(&git_manifest_path).unwrap();
    assert!(
        git_manifest.contains("log") || git_manifest.contains("subcommands"),
        "git manifest should contain enriched description with 'log' or 'subcommands':\n{git_manifest}"
    );

    // Scenario 2: unknown binary gets generic manifest from write_shell_tool_manifests
    let custom_manifest_path = workdir
        .join("tools")
        .join("my-custom-tool")
        .join("murmur.yaml");
    assert!(
        custom_manifest_path.exists(),
        "my-custom-tool manifest should exist (generic fallback)"
    );
    let custom_manifest = fs::read_to_string(&custom_manifest_path).unwrap();
    assert!(
        custom_manifest.contains("command"),
        "my-custom-tool manifest should contain 'command':\n{custom_manifest}"
    );
    assert!(
        !custom_manifest.contains("log") && !custom_manifest.contains("subcommands"),
        "my-custom-tool manifest should not contain git-specific terms:\n{custom_manifest}"
    );
}

#[test]
fn shell_desc_driver_not_declared_falls_back_to_generic() {
    // No driver in artifacts — behavior must be identical to pre-feature baseline.
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project.path().to_path_buf(),
            capsule_name: "test-no-driver".to_string(),
            capsule_version: "0.1.0".to_string(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![],
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: CapabilityPolicy {
                shell_allow: vec!["bash".to_string()],
                ..Default::default()
            },
            inference: Some(InferenceConfig {
                transport: "http".to_string(),
                endpoint: Some("http://localhost:9999".to_string()),
                model: "test-model".to_string(),
                api_key: None,
                driver: Some(InferenceDriver {
                    artifact: "fake-driver".to_string(),
                    config: None,
                }),
                command: None,
                compaction: None,
                system_prompt: None,
                system_prompt_file: None,
                system_prompt_artifact: None,
                max_turns: 10,
                max_task_reopens: 1,
                max_tokens: None,
            }),
            context: None,
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
            job_id: None,
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .expect("stage_session should succeed without shell-desc driver");

    let bash_manifest = fs::read_to_string(
        staged
            .workdir
            .join("tools")
            .join("bash")
            .join("murmur.yaml"),
    )
    .unwrap();
    // Generic manifest has "command" but not the enriched bash description
    assert!(
        bash_manifest.contains("command"),
        "bash manifest should contain 'command':\n{bash_manifest}"
    );
}

#[test]
#[ignore = "requires a default-artifacts checkout with murmur-driver-shell-desc built; set MURMUR_DEFAULT_ARTIFACTS_DIR or clone it next to this repo"]
fn shell_desc_driver_respects_custom_manifest() {
    let binary_path = shell_desc_binary();
    if !binary_path.exists() {
        eprintln!("Skipping custom-manifest test: driver binary not built");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let shell_desc_artifact = common::create_shell_desc_driver_artifact(
        artifact_dir.path(),
        SHELL_DESC_DRIVER_NAME,
        SHELL_DESC_DRIVER_VERSION,
        &binary_path,
    );
    common::publish_local(&home, &shell_desc_artifact).success();

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project.path().to_path_buf(),
            capsule_name: "test-custom-manifest".to_string(),
            capsule_version: "0.1.0".to_string(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![ArtifactRequest {
                name: SHELL_DESC_DRIVER_NAME.to_string(),
                version: SHELL_DESC_DRIVER_VERSION.to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: CapabilityPolicy {
                shell_allow: vec!["git".to_string()],
                ..Default::default()
            },
            inference: Some(InferenceConfig {
                transport: "http".to_string(),
                endpoint: Some("http://localhost:9999".to_string()),
                model: "test-model".to_string(),
                api_key: None,
                driver: Some(InferenceDriver {
                    artifact: "fake-driver".to_string(),
                    config: None,
                }),
                command: None,
                compaction: None,
                system_prompt: None,
                system_prompt_file: None,
                system_prompt_artifact: None,
                max_turns: 10,
                max_task_reopens: 1,
                max_tokens: None,
            }),
            context: None,
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
            job_id: None,
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .expect("stage_session should succeed");

    // The installed artifact manifest for murmur-driver-shell-desc was written first.
    // write_shell_tool_manifests skips it because the manifest already exists.
    // Verify the artifact's own manifest survived unchanged.
    let driver_manifest = fs::read_to_string(
        staged
            .workdir
            .join("tools")
            .join(SHELL_DESC_DRIVER_NAME)
            .join("murmur.yaml"),
    )
    .unwrap();
    assert!(
        driver_manifest.contains("artifact_type: shell-desc-driver"),
        "driver manifest should contain artifact_type:\n{driver_manifest}"
    );
}
