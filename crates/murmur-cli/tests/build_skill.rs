#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;
use zip::ZipArchive;

// ── helpers ──────────────────────────────────────────────────────────────────

fn zip_entries(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();
    names
}

fn zip_entry_content(path: &Path, entry: &str) -> String {
    let file = fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut s = String::new();
    archive
        .by_name(entry)
        .unwrap()
        .read_to_string(&mut s)
        .unwrap();
    s
}

fn find_zip_in(dir: &Path, prefix: &str) -> PathBuf {
    for entry in fs::read_dir(dir).unwrap() {
        let e = entry.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) && name.ends_with(".mur.zip") {
            return e.path();
        }
    }
    panic!(
        "no .mur.zip with prefix {prefix} found in {}",
        dir.display()
    )
}

// ── CLI-level tests ───────────────────────────────────────────────────────────

#[test]
fn skill_build_flag_folder_infers_name() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let skill_dir = src.path().join("my-skill");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), "# My Skill\nGuidance.\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", skill_dir.to_str().unwrap()])
        .current_dir(outdir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    let zip = find_zip_in(outdir.path(), "my-skill");
    let entries = zip_entries(&zip);
    assert!(entries.contains(&"murmur.yaml".to_string()));
    assert!(entries.contains(&"skill.md".to_string()));
    let manifest = zip_entry_content(&zip, "murmur.yaml");
    assert!(manifest.contains("name: my-skill"), "got: {manifest}");
    assert!(manifest.contains("runtime: skill"), "got: {manifest}");
}

#[test]
fn skill_build_flag_explicit_name() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("SKILL.md"), "# Skill\nContent.\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            "--skill",
            "explicit-wrapped",
            src.path().to_str().unwrap(),
        ])
        .current_dir(outdir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    let zip = find_zip_in(outdir.path(), "explicit-wrapped");
    let manifest = zip_entry_content(&zip, "murmur.yaml");
    assert!(
        manifest.contains("name: explicit-wrapped"),
        "got: {manifest}"
    );
}

#[test]
fn skill_build_flag_version_overrides_default() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            "--skill",
            "versioned-skill",
            "--version",
            "3.1.4",
            src.path().to_str().unwrap(),
        ])
        .current_dir(outdir.path())
        .assert()
        .success();

    let zip = outdir.path().join("versioned-skill-3.1.4.mur.zip");
    assert!(zip.exists(), "expected versioned-skill-3.1.4.mur.zip");
    let manifest = zip_entry_content(&zip, "murmur.yaml");
    assert!(manifest.contains("version: '3.1.4'"), "got: {manifest}");
}

#[test]
fn skill_build_missing_skill_md_exits_nonzero() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("README.md"), "just a readme").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", src.path().to_str().unwrap()])
        .current_dir(outdir.path())
        .assert()
        .failure()
        .stderr(predicate::str::is_match("(?i)skill.md").unwrap());

    // No zip produced
    let has_zip = fs::read_dir(outdir.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".mur.zip")
    });
    assert!(
        !has_zip,
        "no zip should be produced when SKILL.md is absent"
    );
}

#[test]
fn skill_build_wrong_runtime_in_manifest_exits_nonzero() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();
    fs::write(
        src.path().join("murmur.yaml"),
        "name: x\nversion: '0.1.0'\nruntime: tool\n",
    )
    .unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", src.path().to_str().unwrap()])
        .current_dir(outdir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("runtime: skill"));
}

#[test]
fn skill_build_pre_authored_manifest_preserved() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();
    fs::write(
        src.path().join("murmur.yaml"),
        "name: pre-authored\nversion: '9.0.0'\nruntime: skill\n",
    )
    .unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", src.path().to_str().unwrap()])
        .current_dir(outdir.path())
        .assert()
        .success();

    // The output zip uses the inferred name and the version from our arg (0.1.0 default)
    // but the MANIFEST CONTENT should be the pre-authored one
    let zip = find_zip_in(outdir.path(), "");
    let manifest = zip_entry_content(&zip, "murmur.yaml");
    assert!(manifest.contains("name: pre-authored"), "got: {manifest}");
    assert!(manifest.contains("version: '9.0.0'"), "got: {manifest}");
    assert!(manifest.contains("runtime: skill"), "got: {manifest}");
}

#[test]
fn skill_build_existing_mur_build_unchanged() {
    // Verify the standard `mur build <path>` behavior is not affected
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("skill-happy");
    let dir = tempdir().unwrap();

    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    for entry in fs::read_dir(&fixture).unwrap() {
        let e = entry.unwrap();
        fs::copy(e.path(), src.join(e.file_name())).unwrap();
    }

    let artifact = src.join("out.mur.zip");
    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            src.to_str().unwrap(),
            "--output",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    assert!(artifact.exists());
}

// ── roundtrip: build → publish → stage → verify skill.md installed ───────────

#[test]
fn skill_build_roundtrip_skill_md_installed_in_workdir() {
    use capsule_runtime::{
        capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
    };
    use murmur_artifact::{
        load_runtime_manifest, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
    };
    use std::collections::HashSet;
    use tempfile::TempDir;

    let home: TempDir = tempdir().unwrap();
    let src: TempDir = tempdir().unwrap();
    let build_out: TempDir = tempdir().unwrap();

    // 1. Create a minimal skill folder with SKILL.md
    fs::write(
        src.path().join("SKILL.md"),
        "# Roundtrip Skill\nDo things.\n",
    )
    .unwrap();

    // 2. Build via `mur build --skill`
    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", "rt-skill", src.path().to_str().unwrap()])
        .current_dir(build_out.path())
        .assert()
        .success();

    let artifact = build_out.path().join("rt-skill-0.1.0.mur.zip");
    assert!(artifact.exists());

    // 3. Publish to local registry
    common::publish_local(&home, &artifact)
        .success()
        .stdout(predicate::str::contains("Published rt-skill@0.1.0"));

    // 4. Verify it's in the registry
    assert!(home
        .path()
        .join(".murmur/artifacts/rt-skill/0.1.0")
        .exists());

    // 5. Stage a capsule that declares the skill artifact.
    // We supply a minimal InferenceConfig so stage_session treats this as an agent
    // capsule (which allows empty WASM bytes). The inference endpoint is never called
    // during staging — only the skill artifact installation path runs.
    let capsule_dir: TempDir = tempdir().unwrap();
    let manifest_content = "name: test-capsule\nversion: 0.1.0\nartifacts:\n  - name: rt-skill\n    version: 0.1.0\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let mut requested_artifacts = Vec::new();
    for a in &runtime_manifest.artifacts {
        requested_artifacts.push(ArtifactRequest {
            name: a.name.clone(),
            version: a.version.clone(),
            runtime: a.runtime.clone(),
            source: a.source.clone(),
            on_overflow: a.on_overflow,
            config: a.config.clone(),
            capabilities: a.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    // Minimal inference config so stage_session accepts empty capsule bytes
    let stub_inference = Some(InferenceConfig {
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
    });

    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    let staged = stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: capsule_dir.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy,
            inference: stub_inference,
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
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
        },
    )
    .unwrap();

    // 6. Verify tools/rt-skill/skill.md is present in the workdir
    let skill_path = staged
        .workdir
        .join("tools")
        .join("rt-skill")
        .join("skill.md");
    assert!(
        skill_path.exists(),
        "tools/rt-skill/skill.md should be present after staging; workdir: {}",
        staged.workdir.display()
    );
    let content = fs::read_to_string(&skill_path).unwrap();
    assert!(
        content.contains("Roundtrip Skill"),
        "skill.md content mismatch: {content}"
    );
}
