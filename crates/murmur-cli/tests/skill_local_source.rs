//! Integration tests for skill local source (`source:` field).
//!
//! A local source is an Agent Skills folder, or its `SKILL.md`, beside the manifest. It is held to
//! the standard as `mur build` holds a skill folder, except that `metadata.version` is not read,
//! and every refusal comes before a workdir exists.

#[path = "common/mod.rs"]
mod common;

use std::{collections::HashSet, fs, path::Path};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
};
use predicates::prelude::*;
use tempfile::{tempdir, TempDir};

/// The S1 skill's `SKILL.md` without its `metadata` block: a local source carries no version.
const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms; use when a form must be filled.\n---\n# PDF helper\nSee references/forms.md.\n";

/// Minimal inference config so stage_session treats the capsule as an agent capsule
/// (which permits empty WASM component bytes). The endpoint is never contacted during staging.
fn stub_inference() -> Option<InferenceConfig> {
    Some(InferenceConfig {
        transport: "http".to_string(),
        model: "test-model".to_string(),
        driver: Some(InferenceDriver {
            artifact: "local/dummy-driver".to_string(),
            config: None,
        }),
        command: None,
        compaction: None,
        system_prompt: None,
        system_prompt_file: None,
        system_prompt_artifact: None,
        instruction_files: None,
        max_turns: Some(10),
        // No driver is staged, so no `models:` table is read: the cap is the manifest's.
        max_tokens: Some(4096),
        max_session_tokens: None,
        tool_refresh: murmur_artifact::ToolRefresh::Compaction,
        alternates: Vec::new(),
        retry: murmur_artifact::InferenceRetry::default(),
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
            config: a.config.clone(),
            gateway: a.gateway.clone(),
            capabilities: a.capabilities.clone(),
        })
        .collect()
}

/// Stage the capsule whose manifest is `capsule_dir/murmur.yaml` against `home`'s global store.
fn stage_capsule_dir(home: &TempDir, capsule_dir: &TempDir) -> capsule_runtime::StagedSession {
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    common::process_home();
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            source_supply: None,
            expected_workdir_identity: None,
            parent_workdir_snapshot: None,
            session_deadline: None,
            manifest_file: None,
            credentials_file: None,
            manifest_dir: capsule_dir.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_artifact: None,
            capsule_component_bytes: Vec::new(),
            artifacts: requested_from(&runtime_manifest),
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy,
            inference: stub_inference(),
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
    .unwrap()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `<capsule_dir>/skills/pdf-helper` holding `skill_md` as `skill_md_name` and
/// `references/forms.md`.
fn pdf_helper_source(
    capsule_dir: &Path,
    skill_md_name: &str,
    skill_md: &str,
) -> std::path::PathBuf {
    let folder = capsule_dir.join("skills").join("pdf-helper");
    fs::create_dir_all(folder.join("references")).unwrap();
    fs::write(folder.join(skill_md_name), skill_md).unwrap();
    fs::write(folder.join("references/forms.md"), "# Forms\n").unwrap();
    folder
}

fn write_skill_manifest(capsule_dir: &Path, source: &str) {
    fs::write(
        capsule_dir.join("murmur.yaml"),
        format!(
            "name: cap\nversion: 0.1.0\nartifacts:\n  - name: pdf-helper\n    source: {source}\n    runtime: skill\n"
        ),
    )
    .unwrap();
}

// ── S5: staging a standard local source ───────────────────────────────────────

/// A folder source installs the whole folder under `tools/<name>/` — `SKILL.md` and every
/// supporting file, never a dot-entry — beside the `murmur.yaml` staging writes, which carries
/// the front block's description. MURMUR.md lists the skill by that description.
#[test]
fn s5_local_source_folder_installs_the_skill_folder() {
    let home: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();
    let folder = pdf_helper_source(capsule_dir.path(), "SKILL.md", PDF_HELPER_SKILL_MD);
    fs::create_dir_all(folder.join(".git")).unwrap();
    fs::write(folder.join(".git/HEAD"), "ref\n").unwrap();
    write_skill_manifest(capsule_dir.path(), "./skills/pdf-helper");

    let staged = stage_capsule_dir(&home, &capsule_dir);

    let tool_dir = staged.runtime_dir.join("tools").join("pdf-helper");
    assert_eq!(
        files_in(&tool_dir),
        ["SKILL.md", "murmur.yaml", "references"]
    );
    assert_eq!(
        fs::read_to_string(tool_dir.join("SKILL.md")).unwrap(),
        PDF_HELPER_SKILL_MD
    );
    assert_eq!(
        fs::read_to_string(tool_dir.join("references/forms.md")).unwrap(),
        "# Forms\n"
    );
    let installed: serde_norway::Value =
        serde_norway::from_str(&fs::read_to_string(tool_dir.join("murmur.yaml")).unwrap()).unwrap();
    assert_eq!(installed["name"].as_str(), Some("pdf-helper"));
    assert_eq!(
        installed["description"].as_str(),
        Some("Fill PDF forms; use when a form must be filled.")
    );

    let murmur_md = fs::read_to_string(staged.runtime_dir.join("MURMUR.md")).unwrap();
    let line = murmur_md
        .lines()
        .find(|line| line.contains("**pdf-helper**") && line.contains("call by name"))
        .unwrap_or_else(|| panic!("MURMUR.md lists no pdf-helper:\n{murmur_md}"));
    assert!(
        line.contains("**pdf-helper** — Fill PDF forms; use when a form must be filled."),
        "{line}"
    );
    assert!(
        line.contains(&format!("{}", tool_dir.join("SKILL.md").display())),
        "MURMUR.md should name {}: {line}",
        tool_dir.join("SKILL.md").display()
    );
}

#[test]
fn s5_local_source_file_installs_that_file_alone() {
    let home: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();
    pdf_helper_source(capsule_dir.path(), "SKILL.md", PDF_HELPER_SKILL_MD);
    write_skill_manifest(capsule_dir.path(), "./skills/pdf-helper/SKILL.md");

    let runtime_manifest = load_runtime_manifest(&capsule_dir.path().join("murmur.yaml")).unwrap();
    assert_eq!(runtime_manifest.artifacts[0].version, "local");

    let staged = stage_capsule_dir(&home, &capsule_dir);

    let tool_dir = staged.runtime_dir.join("tools").join("pdf-helper");
    assert_eq!(files_in(&tool_dir), ["SKILL.md", "murmur.yaml"]);
    assert_eq!(
        fs::read_to_string(tool_dir.join("SKILL.md")).unwrap(),
        PDF_HELPER_SKILL_MD
    );
}

#[test]
fn local_source_coexists_with_registry_skill() {
    let home: TempDir = tempdir().unwrap();
    let build_out: TempDir = tempdir().unwrap();
    let src: TempDir = tempdir().unwrap();
    let capsule_dir: TempDir = tempdir().unwrap();

    let registry_skill = src.path().join("reg-skill");
    fs::create_dir_all(&registry_skill).unwrap();
    fs::write(
        registry_skill.join("SKILL.md"),
        "---\nname: reg-skill\ndescription: A registry skill.\nmetadata:\n  version: \"0.1.0\"\n---\n# Registry Skill\n",
    )
    .unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", registry_skill.to_str().unwrap()])
        .current_dir(build_out.path())
        .assert()
        .success();
    let artifact = build_out.path().join("reg-skill-0.1.0.mur.zip");
    common::publish_local(&home, &artifact).success();

    let local_skill = capsule_dir.path().join("local-skill");
    fs::create_dir_all(&local_skill).unwrap();
    fs::write(
        local_skill.join("SKILL.md"),
        "---\nname: local-skill\ndescription: A local skill.\n---\n# Local Skill\n",
    )
    .unwrap();

    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        "name: cap\nversion: 0.1.0\nartifacts:\n  - name: local/reg-skill\n    version: 0.1.0\n    runtime: skill\n  - name: local-skill\n    source: ./local-skill/\n    runtime: skill\n",
    )
    .unwrap();

    let staged = stage_capsule_dir(&home, &capsule_dir);

    assert!(staged.runtime_dir.join("tools/reg-skill/SKILL.md").exists());
    assert!(staged
        .runtime_dir
        .join("tools/local-skill/SKILL.md")
        .exists());
}

// ── Refusals before any workdir exists ────────────────────────────────────────

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

    assert!(!capsule_dir.path().join("workdir").exists());
}

/// A home with `dummy-driver@0.1.0` published, so a `mur run` of a manifest naming it as the
/// inference driver gets past the installed-artifact check and reaches staging.
fn home_with_dummy_driver() -> TempDir {
    let home = tempdir().unwrap();
    let artifacts = tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        "dummy-driver",
        "0.1.0",
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    home
}

/// A capsule whose one skill, `name`, comes from `source`, with an inference driver so that a
/// `mur run` reaches staging.
fn run_manifest(name: &str, source: &str) -> String {
    format!(
        "name: cap\nversion: 0.1.0\ninference:\n  transport: http\n  model: test\n  driver:\n    artifact: local/dummy-driver\nartifacts:\n  - name: {name}\n    source: {source}\n    runtime: skill\n  - name: local/dummy-driver\n    version: 0.1.0\n    runtime: driver\n    gateway:\n      endpoint: http://localhost:9999\n      keyless: true\n"
    )
}

/// `mur run` of `capsule_dir/murmur.yaml` against a home holding the driver: it must fail, and
/// leave no workdir behind. Returns its stderr.
fn run_refused(capsule_dir: &TempDir) -> String {
    let home = home_with_dummy_driver();
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    let assert = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure();
    assert!(!capsule_dir.path().join("workdir").exists());
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

#[test]
fn source_path_missing_fails_naming_path() {
    let capsule_dir: TempDir = tempdir().unwrap();
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("ghost", "./skills/ghost/SKILL.md"),
    )
    .unwrap();

    let stderr = run_refused(&capsule_dir);
    assert!(stderr.contains("skill source path not found"), "{stderr}");
    assert!(stderr.contains("ghost"), "{stderr}");
}

#[test]
fn source_directory_without_skill_md_fails() {
    let capsule_dir: TempDir = tempdir().unwrap();
    fs::create_dir_all(capsule_dir.path().join("skills").join("empty")).unwrap();
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("empty", "./skills/empty/"),
    )
    .unwrap();

    let stderr = run_refused(&capsule_dir);
    assert!(stderr.contains("contains no SKILL.md"), "{stderr}");
    assert!(stderr.contains("empty"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn local_source_directory_with_a_symlink_fails_before_staging() {
    let capsule_dir: TempDir = tempdir().unwrap();
    let skill_dir = capsule_dir.path().join("skills").join("linked");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: linked\ndescription: Linked.\n---\n# Linked\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", skill_dir.join("passwd")).unwrap();
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("linked", "./skills/linked"),
    )
    .unwrap();

    let stderr = run_refused(&capsule_dir);
    assert!(stderr.contains("E-BLD-002"), "{stderr}");
    assert!(stderr.contains("passwd"), "{stderr}");
}

/// Asserts `stderr` is an `E-BLD-009` refusal holding `message` and `hint`.
fn assert_e_bld_009(stderr: &str, message: &str, hint: &str) {
    assert!(stderr.contains("error[E-BLD-009]: "), "{stderr}");
    assert!(
        stderr.contains(message),
        "stderr lacks {message:?}:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("hint: {hint}")),
        "stderr lacks hint {hint:?}:\n{stderr}"
    );
}

#[test]
fn s5_a_misspelled_skill_md_in_a_folder_source_is_refused() {
    let capsule_dir: TempDir = tempdir().unwrap();
    pdf_helper_source(capsule_dir.path(), "skill.md", PDF_HELPER_SKILL_MD);
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("pdf-helper", "./skills/pdf-helper"),
    )
    .unwrap();

    assert_e_bld_009(
        &run_refused(&capsule_dir),
        "skills/pdf-helper/skill.md: a skill's file is named SKILL.md, exactly",
        "rename skill.md to SKILL.md",
    );
}

#[test]
fn s5_a_misspelled_file_source_is_refused() {
    let capsule_dir: TempDir = tempdir().unwrap();
    pdf_helper_source(capsule_dir.path(), "skill.md", PDF_HELPER_SKILL_MD);
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("pdf-helper", "./skills/pdf-helper/skill.md"),
    )
    .unwrap();

    assert_e_bld_009(
        &run_refused(&capsule_dir),
        "skills/pdf-helper/skill.md: a skill's file is named SKILL.md, exactly",
        "rename skill.md to SKILL.md",
    );
}

#[test]
fn s5_a_murmur_yaml_in_a_folder_source_is_refused() {
    let capsule_dir: TempDir = tempdir().unwrap();
    let folder = pdf_helper_source(capsule_dir.path(), "SKILL.md", PDF_HELPER_SKILL_MD);
    fs::write(
        folder.join("murmur.yaml"),
        "name: pdf-helper\nversion: 9.9.9\nruntime: skill\n",
    )
    .unwrap();
    fs::write(
        capsule_dir.path().join("murmur.yaml"),
        run_manifest("pdf-helper", "./skills/pdf-helper"),
    )
    .unwrap();

    assert_e_bld_009(
        &run_refused(&capsule_dir),
        "skills/pdf-helper/murmur.yaml: a skill folder holds no murmur.yaml beside SKILL.md",
        "a skill's name, description and version go in the SKILL.md front block; delete \
         murmur.yaml",
    );
}

#[test]
fn s5_a_front_block_name_unlike_the_folder_is_refused() {
    for source in ["./skills/pdf-helper", "./skills/pdf-helper/SKILL.md"] {
        let capsule_dir: TempDir = tempdir().unwrap();
        pdf_helper_source(
            capsule_dir.path(),
            "SKILL.md",
            &PDF_HELPER_SKILL_MD.replace("name: pdf-helper", "name: other-name"),
        );
        fs::write(
            capsule_dir.path().join("murmur.yaml"),
            run_manifest("pdf-helper", source),
        )
        .unwrap();

        let stderr = run_refused(&capsule_dir);
        assert!(stderr.contains("error[E-BLD-009]: "), "{source}: {stderr}");
        assert!(
            stderr.contains(
                "skills/pdf-helper/SKILL.md: front block name 'other-name' differs from the \
                 folder name 'pdf-helper'"
            ),
            "{source}: {stderr}"
        );
        assert!(stderr.contains("hint: "), "{source}: {stderr}");
    }
}
