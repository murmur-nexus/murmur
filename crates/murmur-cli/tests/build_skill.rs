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
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
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

/// A skill's own name carries no scope: the scope comes from the source it is published from.
#[test]
fn skill_build_flag_refuses_a_scoped_name() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", "--skill", "ana/x", src.path().to_str().unwrap()])
        .current_dir(outdir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "error[E-BLD-001]: invalid artifact name 'ana/x': an artifact's own name carries no \
             scope, so it may not contain '/'",
        ))
        .stderr(predicate::str::contains(
            "hint: the scope belongs to the source the artifact is published from, not to the \
             artifact: name it 'x', and it installs as <owner>/x from github.com/<owner> or as \
             local/x from its .mur.zip file",
        ));

    let has_zip = fs::read_dir(outdir.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".mur.zip")
    });
    assert!(!has_zip, "a refused build writes no zip");
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
        .stdout(predicate::str::contains("Published local/rt-skill@0.1.0"));

    // 4. Verify it's in the registry, filed in the `local` scope under its packed name
    assert!(home
        .path()
        .join(".murmur/artifacts/local/rt-skill/0.1.0/rt-skill-0.1.0.mur.zip")
        .exists());

    // 5. Stage a capsule that declares the skill artifact.
    // We supply a minimal InferenceConfig so stage_session treats this as an agent
    // capsule (which allows empty WASM bytes). The inference endpoint is never called
    // during staging — only the skill artifact installation path runs.
    let capsule_dir: TempDir = tempdir().unwrap();
    let manifest_content = "name: test-capsule\nversion: 0.1.0\nartifacts:\n  - name: local/rt-skill\n    version: 0.1.0\n    runtime: skill\n";
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_content).unwrap();

    let staged = stage_capsule(&home, capsule_dir.path(), &manifest_path);

    // 6. Verify tools/rt-skill/skill.md is present in the runtime directory: the model-facing directory
    // carries the bare name
    let skill_path = staged
        .runtime_dir
        .join("tools")
        .join("rt-skill")
        .join("skill.md");
    assert!(
        skill_path.exists(),
        "tools/rt-skill/skill.md should be present after staging; workdir: {}",
        staged.accessible_workdir.display()
    );
    let content = fs::read_to_string(&skill_path).unwrap();
    assert!(
        content.contains("Roundtrip Skill"),
        "skill.md content mismatch: {content}"
    );
}

/// Stage the capsule at `capsule_dir` (its manifest at `manifest_path`) against `home`'s global
/// store, with a minimal inference config so stage_session accepts empty capsule bytes. The
/// inference endpoint is never called during staging — only artifact installation runs.
fn stage_capsule(
    home: &tempfile::TempDir,
    capsule_dir: &Path,
    manifest_path: &Path,
) -> capsule_runtime::StagedSession {
    use capsule_runtime::{
        capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
    };
    use murmur_artifact::{
        load_runtime_manifest, ContainmentClass, InferenceConfig, InferenceDriver, LocalRegistry,
    };
    use std::collections::HashSet;

    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut requested_artifacts = Vec::new();
    for a in &runtime_manifest.artifacts {
        requested_artifacts.push(ArtifactRequest {
            name: a.name.clone(),
            version: a.version.clone(),
            runtime: a.runtime.clone(),
            source: a.source.clone(),
            on_overflow: a.on_overflow,
            config: a.config.clone(),
            gateway: a.gateway.clone(),
            capabilities: a.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    // Minimal inference config so stage_session accepts empty capsule bytes
    let stub_inference = Some(InferenceConfig {
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
    });

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
            manifest_dir: capsule_dir.to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_artifact: None,
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

// ── manifest-less skill folders (the Claude Code / Codex / pi layout) ────────

const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms: use when a form must be filled.\n---\n# PDF helper\nSee references/forms.md.\n";

/// A standard skill folder under `base`: `Some Folder/SKILL.md` with a front block and
/// `Some Folder/references/forms.md`, no murmur.yaml.
fn pdf_helper_folder(base: &Path) -> PathBuf {
    let folder = base.join("Some Folder");
    fs::create_dir_all(folder.join("references")).unwrap();
    fs::write(folder.join("SKILL.md"), PDF_HELPER_SKILL_MD).unwrap();
    fs::write(folder.join("references/forms.md"), "# Forms\n").unwrap();
    folder
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn manifest_yaml(zip: &Path) -> serde_norway::Value {
    serde_norway::from_str(&zip_entry_content(zip, "murmur.yaml")).unwrap()
}

fn mur_build(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .arg("build")
        .args(args)
        .current_dir(cwd)
        .assert()
}

#[test]
fn manifest_less_skill_folder_builds_from_its_front_block() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());

    mur_build(outdir.path(), &[folder.to_str().unwrap()])
        .success()
        .stdout(
            predicate::str::is_match(
                r"^Built artifact: .*/pdf-helper-0\.1\.0\.mur\.zip \(WIT level \d+\)\n$",
            )
            .unwrap(),
        );

    let zip = outdir.path().join("pdf-helper-0.1.0.mur.zip");
    assert_eq!(
        zip_entries(&zip),
        [
            "murmur-build.json",
            "murmur.yaml",
            "references/forms.md",
            "skill.md"
        ]
    );
    assert_eq!(zip_entry_content(&zip, "skill.md"), PDF_HELPER_SKILL_MD);
    assert_eq!(zip_entry_content(&zip, "references/forms.md"), "# Forms\n");
    let manifest = manifest_yaml(&zip);
    assert_eq!(manifest["name"].as_str(), Some("pdf-helper"));
    assert_eq!(manifest["version"].as_str(), Some("0.1.0"));
    assert_eq!(manifest["runtime"].as_str(), Some("skill"));
    assert_eq!(
        manifest["description"].as_str(),
        Some("Fill PDF forms: use when a form must be filled.")
    );
    // Nothing is written into the source folder.
    assert_eq!(files_in(&folder), ["SKILL.md", "references"]);
    assert_eq!(files_in(outdir.path()), ["pdf-helper-0.1.0.mur.zip"]);
}

#[test]
fn folder_without_manifest_or_skill_md_still_fails_as_before() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::write(src.path().join("README.md"), "just a readme").unwrap();

    mur_build(outdir.path(), &[src.path().to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("E-IO-001"))
        .stderr(predicate::str::contains("murmur.yaml not found at"));

    assert_eq!(files_in(src.path()), ["README.md"]);
    assert!(files_in(outdir.path()).is_empty());
}

#[test]
fn manifest_backed_skill_folder_packs_only_requires_files() {
    let src = tempdir().unwrap();
    fs::create_dir_all(src.path().join("references")).unwrap();
    fs::write(
        src.path().join("murmur.yaml"),
        "name: curated-skill\nversion: 0.2.0\nruntime: skill\n",
    )
    .unwrap();
    fs::write(src.path().join("skill.md"), "# Curated\n").unwrap();
    fs::write(src.path().join("references/x.md"), "# X\n").unwrap();

    mur_build(src.path(), &[src.path().to_str().unwrap()]).success();
    let zip = src.path().join("curated-skill-0.2.0.mur.zip");
    assert_eq!(
        zip_entries(&zip),
        ["murmur-build.json", "murmur.yaml", "skill.md"]
    );

    fs::remove_file(&zip).unwrap();
    fs::remove_file(src.path().join("skill.md")).unwrap();
    mur_build(src.path(), &[src.path().to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("E-IO-003"))
        .stderr(predicate::str::contains("missing required file 'skill.md'"));
}

#[test]
fn skill_flags_override_the_front_block() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());

    mur_build(
        outdir.path(),
        &[
            "--skill",
            "other-name",
            "--summary",
            "Short",
            folder.to_str().unwrap(),
        ],
    )
    .success();

    let zip = outdir.path().join("other-name-0.1.0.mur.zip");
    let manifest = manifest_yaml(&zip);
    assert_eq!(manifest["name"].as_str(), Some("other-name"));
    assert_eq!(manifest["description"].as_str(), Some("Short"));
    assert_eq!(files_in(outdir.path()), ["other-name-0.1.0.mur.zip"]);
}

#[test]
fn front_block_without_name_falls_back_to_the_folder_name() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = base.path().join("pdf-tools");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("SKILL.md"),
        "---\ndescription: \"Tools for PDFs\"\nlicense: MIT\n---\n# Tools\n",
    )
    .unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    let manifest = manifest_yaml(&outdir.path().join("pdf-tools-0.1.0.mur.zip"));
    assert_eq!(manifest["name"].as_str(), Some("pdf-tools"));
    assert_eq!(manifest["description"].as_str(), Some("Tools for PDFs"));
}

#[test]
fn front_block_name_that_is_not_an_artifact_name_is_refused() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = base.path().join("pdf");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("SKILL.md"),
        "---\nname: PDF_Helper\n---\n# PDF\n",
    )
    .unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("E-BLD-001"))
        .stderr(predicate::str::contains(
            "invalid artifact name 'PDF_Helper'",
        ))
        .stderr(predicate::str::contains("pass --skill <name>"));

    assert!(files_in(outdir.path()).is_empty());
    assert_eq!(files_in(&folder), ["SKILL.md"]);
}

#[test]
fn front_block_folded_description_is_written_as_yaml() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = base.path().join("folded");
    fs::create_dir_all(&folder).unwrap();
    fs::write(
        folder.join("SKILL.md"),
        "---\nname: folded\ndescription: >\n  Quotes ' and \" and: colons\n  on two lines.\n---\n# F\n",
    )
    .unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    let zip = outdir.path().join("folded-0.1.0.mur.zip");
    let manifest_text = zip_entry_content(&zip, "murmur.yaml");
    assert!(
        manifest_text.starts_with("name: folded\nversion: '0.1.0'\nruntime: skill\ndescription: "),
        "got: {manifest_text}"
    );
    assert_eq!(
        manifest_yaml(&zip)["description"].as_str(),
        Some("Quotes ' and \" and: colons on two lines.")
    );
}

#[test]
fn manifest_less_build_skips_dot_entries_and_built_artifacts() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::create_dir_all(folder.join(".git")).unwrap();
    fs::write(folder.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(folder.join(".DS_Store"), "x").unwrap();
    fs::write(folder.join("old-0.0.1.mur.zip"), "stale").unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    assert_eq!(
        zip_entries(&outdir.path().join("pdf-helper-0.1.0.mur.zip")),
        [
            "murmur-build.json",
            "murmur.yaml",
            "references/forms.md",
            "skill.md"
        ]
    );
}

fn assert_refused_with_no_output(folder: &Path, outdir: &Path, names: &str) {
    mur_build(outdir, &[folder.to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("E-BLD-002"))
        .stderr(predicate::str::contains(names));
    assert!(files_in(outdir).is_empty(), "{:?}", files_in(outdir));
    assert!(
        !files_in(folder)
            .iter()
            .any(|name| name.ends_with(".tmp") || name.ends_with(".mur.zip")),
        "{:?}",
        files_in(folder)
    );
}

#[cfg(unix)]
#[test]
fn manifest_less_build_refuses_a_symlink() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    std::os::unix::fs::symlink("forms.md", folder.join("references/link.md")).unwrap();

    assert_refused_with_no_output(&folder, outdir.path(), "references/link.md");
}

#[test]
fn manifest_less_build_refuses_two_spellings_of_skill_md() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::write(folder.join("skill.md"), "# other\n").unwrap();

    assert_refused_with_no_output(&folder, outdir.path(), "both SKILL.md and skill.md");
}

#[test]
fn manifest_less_build_refuses_an_authored_build_record() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::write(folder.join("murmur-build.json"), "{\"built_at_level\":9}").unwrap();

    assert_refused_with_no_output(&folder, outdir.path(), "murmur-build.json");
}

#[test]
fn skill_build_output_names_a_directory_or_the_file() {
    let base = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    let dist = base.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    mur_build(
        cwd.path(),
        &[folder.to_str().unwrap(), "--output", dist.to_str().unwrap()],
    )
    .success();
    assert_eq!(files_in(&dist), ["pdf-helper-0.1.0.mur.zip"]);

    // An output file inside the folder, under a name the walk does not skip, is not packed
    // into the next build.
    let inside = folder.join("artifact.zip");
    for _ in 0..2 {
        mur_build(
            cwd.path(),
            &[folder.to_str().unwrap(), "-o", inside.to_str().unwrap()],
        )
        .success()
        .stdout(predicate::str::contains("artifact.zip"));
    }
    assert_eq!(
        zip_entries(&inside),
        [
            "murmur-build.json",
            "murmur.yaml",
            "references/forms.md",
            "skill.md"
        ]
    );
    assert!(files_in(cwd.path()).is_empty());
}

#[test]
fn skill_flag_with_a_manifest_packs_the_folder_recursively() {
    let src = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    fs::create_dir_all(src.path().join("references")).unwrap();
    let manifest = "name: authored\nversion: '1.2.3'\nruntime: skill\n";
    fs::write(src.path().join("murmur.yaml"), manifest).unwrap();
    fs::write(
        src.path().join("SKILL.md"),
        "---\nname: ignored\n---\n# A\n",
    )
    .unwrap();
    fs::write(src.path().join("references/x.md"), "# X\n").unwrap();

    mur_build(
        outdir.path(),
        &["--skill", "authored", src.path().to_str().unwrap()],
    )
    .success();

    let zip = outdir.path().join("authored-0.1.0.mur.zip");
    assert_eq!(
        zip_entries(&zip),
        [
            "murmur-build.json",
            "murmur.yaml",
            "references/x.md",
            "skill.md"
        ]
    );
    assert_eq!(zip_entry_content(&zip, "murmur.yaml"), manifest);
}

#[test]
fn skill_build_roundtrip_installs_supporting_files() {
    let home = tempdir().unwrap();
    let base = tempdir().unwrap();
    let build_out = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());

    mur_build(build_out.path(), &[folder.to_str().unwrap()]).success();
    let artifact = build_out.path().join("pdf-helper-0.1.0.mur.zip");
    common::publish_local(&home, &artifact)
        .success()
        .stdout(predicate::str::contains("Published local/pdf-helper@0.1.0"));

    let capsule_dir = tempdir().unwrap();
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        "name: test-capsule\nversion: 0.1.0\nartifacts:\n  - name: local/pdf-helper\n    version: 0.1.0\n    runtime: skill\n",
    )
    .unwrap();
    let staged = stage_capsule(&home, capsule_dir.path(), &manifest_path);

    let tool_dir = staged.runtime_dir.join("tools").join("pdf-helper");
    assert_eq!(
        fs::read_to_string(tool_dir.join("skill.md")).unwrap(),
        PDF_HELPER_SKILL_MD
    );
    assert_eq!(
        fs::read_to_string(tool_dir.join("references/forms.md")).unwrap(),
        "# Forms\n"
    );
    assert_eq!(
        files_in(&tool_dir),
        ["murmur.yaml", "references", "skill.md"]
    );
    let installed: serde_norway::Value =
        serde_norway::from_str(&fs::read_to_string(tool_dir.join("murmur.yaml")).unwrap()).unwrap();
    assert_eq!(installed["name"].as_str(), Some("pdf-helper"));
}
