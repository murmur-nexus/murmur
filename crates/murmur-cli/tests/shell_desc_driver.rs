#[path = "common/mod.rs"]
mod common;

use std::{collections::HashSet, fs};

use capsule_runtime::{stage_session, CapabilityPolicy, StageRequest};
use murmur_artifact::{ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry};

#[test]
fn shell_desc_driver_not_declared_falls_back_to_generic() {
    if common::skip_without_host_support("shell_desc_driver_not_declared_falls_back_to_generic") {
        return;
    }
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
            credentials_file: None,
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
                model: "test-model".to_string(),
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
                max_tokens: None,
                max_session_tokens: None,
                tool_refresh: murmur_artifact::ToolRefresh::Compaction,
                alternates: Vec::new(),
            }),
            system_prompt_overridden: false,
            context: None,
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
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
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
