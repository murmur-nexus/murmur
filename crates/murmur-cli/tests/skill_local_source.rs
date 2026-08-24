//! Integration tests for skill local source (`source:` field).
//!
//! Covers: local-source staging from a file path and a directory, coexistence with a
//! registry skill, and the fail-fast failure modes (source on non-skill runtime, missing
//! path, directory without skill.md).

#[path = "common/mod.rs"]
mod common;

use std::{collections::HashSet, fs};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
};
use predicates::prelude::*;
use tempfile::{tempdir, TempDir};

/// Minimal inference config so stage_session treats the capsule as an agent capsule
/// (which permits empty WASM component bytes). The endpoint is never contacted during staging.
fn stub_inference() -> Option<InferenceConfig> {
    Some(InferenceConfig {
        transport: "http".to_string(),
        endpoint: Some("http://localhost:9999".to_string()),
        model: "test-model".to_string(),
        api_key: None,
        driver: Some(InferenceDriver {
            artifact: "dummy-driver".to_string(),
            config: None,
        }),
        command: None,
        compaction: None,
        system_prompt: None,
        system_prompt_file: None,
        system_prompt_artifact: None,
        max_turns: 10,
        max_tokens: None,
    })
}

fn requested_from(manifest: &murmur_artifact::RuntimeManifest) -> Vec<ArtifactRequest> {
    manifest
        .artifacts
        .iter()
        .map(|a| ArtifactRequest {
            name: a.name.clone(),
            version: a.version.clone(),
            runtime: a.runtime.clone(),
            source: a.source.clone(),
            on_overflow: a.on_overflow,
            capabilities: a.capabilities.clone(),
        })
        .collect()
}

#[test]
fn local_source_file_path_installs_skill_md() {
    let home: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();

    // Author a skill.md directly in the capsule dir; reference it by file path.
    let skill_md = capsule_dir.path().join("skills").join("my-skill");
    fs::create_dir_all(&skill_md).unwrap();
    fs::write(skill_md.join("skill.md"), "# Local Skill\nEdit me live.\n").unwrap();

    let manifest_content = "name: cap\nversion: 0.1.0\nartifacts:\n  - name: my-skill\n    source: ./skills/my-skill/skill.md\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    assert_eq!(runtime_manifest.artifacts[0].version, "local");

    // Empty registry — a local-source skill must not require any published artifact.
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: capsule_dir.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_from(&runtime_manifest),
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy,
            inference: stub_inference(),
            system_prompt_overridden: false,
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
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .unwrap();

    let installed = staged
        .workdir
        .join("tools")
        .join("my-skill")
        .join("skill.md");
    assert!(installed.exists(), "skill.md not installed");
    assert_eq!(
        fs::read_to_string(&installed).unwrap(),
        "# Local Skill\nEdit me live.\n"
    );
}

#[test]
fn local_source_directory_path_finds_skill_md_case_insensitively() {
    let home: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();

    let skill_dir = capsule_dir.path().join("skills").join("other");
    fs::create_dir_all(&skill_dir).unwrap();
    // Uppercase filename — directory lookup is case-insensitive.
    fs::write(skill_dir.join("SKILL.MD"), "# Dir Skill\n").unwrap();

    let manifest_content = "name: cap\nversion: 0.1.0\nartifacts:\n  - name: other\n    source: ./skills/other/\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: capsule_dir.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_from(&runtime_manifest),
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy,
            inference: stub_inference(),
            system_prompt_overridden: false,
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
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .unwrap();

    let installed = staged.workdir.join("tools").join("other").join("skill.md");
    assert!(installed.exists(), "skill.md not installed from directory");
    assert_eq!(fs::read_to_string(&installed).unwrap(), "# Dir Skill\n");
}

#[test]
fn local_source_coexists_with_registry_skill() {
    let home: TempDir = tempdir().unwrap();
    let build_out: TempDir = tempdir().unwrap();
    let src: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();

    // Build + publish a registry skill.
    fs::write(src.path().join("SKILL.md"), "# Registry Skill\n").unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            "--skill",
            "reg-skill",
            src.path().to_str().unwrap(),
        ])
        .current_dir(build_out.path())
        .assert()
        .success();
    let artifact = build_out.path().join("reg-skill-0.1.0.mur.zip");
    common::publish_local(&home, &artifact).success();

    // Author a local-source skill next to the manifest.
    let local_skill = capsule_dir.path().join("local");
    fs::create_dir_all(&local_skill).unwrap();
    fs::write(local_skill.join("skill.md"), "# Local Skill\n").unwrap();

    let manifest_content = "name: cap\nversion: 0.1.0\nartifacts:\n  - name: reg-skill\n    version: 0.1.0\n    runtime: skill\n  - name: local-skill\n    source: ./local/\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: capsule_dir.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_from(&runtime_manifest),
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy,
            inference: stub_inference(),
            system_prompt_overridden: false,
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
            declared_containment_floor: ContainmentClass::Advisory,
        },
    )
    .unwrap();

    assert!(staged
        .workdir
        .join("tools")
        .join("reg-skill")
        .join("skill.md")
        .exists());
    assert!(staged
        .workdir
        .join("tools")
        .join("local-skill")
        .join("skill.md")
        .exists());
}

#[test]
fn source_on_tool_runtime_fails_fast() {
    let capsule_dir: TempDir = tempdir().unwrap();
    let manifest_content = "name: cap\nversion: 0.1.0\nartifacts:\n  - name: my-tool\n    source: ./tools/my-tool\n    runtime: tool\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("'local_source: true'"))
        .stderr(predicate::str::contains("my-tool"));

    // No workdir was created.
    assert!(!capsule_dir.path().join("workdir").exists());
}

#[test]
fn source_path_missing_fails_naming_path() {
    let capsule_dir: TempDir = tempdir().unwrap();
    // Skill source points at a path that does not exist. Inference present so staging is reached.
    let manifest_content = "name: cap\nversion: 0.1.0\ninference:\n  transport: http\n  endpoint: http://localhost:9999\n  model: test\n  driver:\n    artifact: dummy-driver\nartifacts:\n  - name: ghost\n    source: ./skills/ghost/skill.md\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("skill source path not found")
                .and(predicate::str::contains("ghost")),
        );

    assert!(!capsule_dir.path().join("workdir").exists());
}

#[test]
fn source_directory_without_skill_md_fails() {
    let capsule_dir: TempDir = tempdir().unwrap();
    let empty = capsule_dir.path().join("skills").join("empty");
    fs::create_dir_all(&empty).unwrap();

    let manifest_content = "name: cap\nversion: 0.1.0\ninference:\n  transport: http\n  endpoint: http://localhost:9999\n  model: test\n  driver:\n    artifact: dummy-driver\nartifacts:\n  - name: empty-skill\n    source: ./skills/empty/\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("contains no skill.md").and(predicate::str::contains("empty")),
        );

    assert!(!capsule_dir.path().join("workdir").exists());
}
