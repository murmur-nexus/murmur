//! The host reading a staged session is judged against is taken per session, not per process.
//!
//! `mur eval` stages once per dataset case inside a single process, so this is the shape that
//! decides whether a case is judged against the host as it is when that case runs or against a
//! reading taken before the first case did.

use std::{collections::HashSet, fs, sync::Arc};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, HostProbe,
    StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
    RuntimeManifest,
};
use tempfile::{tempdir, TempDir};

/// Minimal inference config so `stage_session` treats the capsule as an agent capsule (which
/// permits empty WASM component bytes). The endpoint is never contacted during staging.
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

fn requested_from(manifest: &RuntimeManifest) -> Vec<ArtifactRequest> {
    manifest
        .artifacts
        .iter()
        .map(|artifact| ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        })
        .collect()
}

fn stage_request(capsule_dir: &TempDir, manifest: &RuntimeManifest) -> StageRequest {
    StageRequest {
        manifest_dir: capsule_dir.path().to_path_buf(),
        capsule_name: manifest.name.clone(),
        capsule_version: manifest.version.clone(),
        capsule_component_bytes: Vec::new(),
        artifacts: requested_from(manifest),
        allowlisted_tools: HashSet::new(),
        lock_expectations: None,
        capability_policy: capability_policy_from_runtime_manifest(manifest),
        inference: stub_inference(),
        system_prompt_overridden: false,
        context: None,
        context_id: None,
        resume: None,
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
        spawn_grant: None,
    }
}

/// Exactly one reading per staging: not zero for the second (which is what a process-lifetime
/// cache produces), and not three for either (which is what letting the tier, the blocker and the
/// grant each ask the host separately inside one `stage_session` would produce).
#[test]
fn staging_twice_in_one_process_takes_one_probe_each() {
    let home: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();

    let skill_dir = capsule_dir.path().join("skills").join("my-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("skill.md"), "# Local Skill\n").unwrap();

    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        "name: cap\nversion: 0.1.0\nartifacts:\n  - name: my-skill\n    source: ./skills/my-skill/skill.md\n    runtime: skill\n",
    )
    .unwrap();
    let manifest = load_runtime_manifest(&manifest_path).unwrap();

    // Empty registry — a local-source skill requires no published artifact.
    let registry = || {
        Arc::new(LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        ))
    };

    let before_first = HostProbe::probes_taken();
    let first = stage_session(registry(), stage_request(&capsule_dir, &manifest)).unwrap();
    let after_first = HostProbe::probes_taken();
    let second = stage_session(registry(), stage_request(&capsule_dir, &manifest)).unwrap();
    let after_second = HostProbe::probes_taken();

    assert_eq!(
        after_first - before_first,
        1,
        "one staged session reads the host exactly once",
    );
    assert_eq!(
        after_second - after_first,
        1,
        "a second staging in the same process reads the host again",
    );
    assert_eq!(
        first.scope_report().achieved_containment,
        second.scope_report().achieved_containment,
        "an unchanged host must be reported identically by both sessions",
    );
}
