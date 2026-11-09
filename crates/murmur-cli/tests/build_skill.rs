//! `mur build <folder>` on an Agent Skills folder: a folder named for the skill holding `SKILL.md`,
//! whose front block alone names, describes and versions the artifact.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::{tempdir, TempDir};
use zip::ZipArchive;

const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms; use when a form must be filled.\nmetadata:\n  version: \"1.2.0\"\n---\n# PDF helper\nSee references/forms.md.\n";

const PDF_HELPER_ZIP: &str = "pdf-helper-1.2.0.mur.zip";

const PDF_HELPER_ENTRIES: [&str; 4] = [
    "SKILL.md",
    "murmur-build.json",
    "murmur.yaml",
    "references/forms.md",
];

fn zip_entries(path: &Path) -> Vec<String> {
    let file = fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
        .collect();
    names.sort();
    names
}

fn zip_entry_bytes(path: &Path, entry: &str) -> Vec<u8> {
    let file = fs::File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name(entry)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn zip_entry_content(path: &Path, entry: &str) -> String {
    String::from_utf8(zip_entry_bytes(path, entry)).unwrap()
}

fn manifest_yaml(zip: &Path) -> serde_norway::Value {
    serde_norway::from_str(&zip_entry_content(zip, "murmur.yaml")).unwrap()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn mur_build(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .arg("build")
        .args(args)
        .current_dir(cwd)
        .assert()
}

/// `<base>/<folder>` holding `SKILL.md` with `skill_md`.
fn skill_folder(base: &Path, folder: &str, skill_md: &str) -> PathBuf {
    let dir = base.join(folder);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    dir
}

/// The S1 skill: `<base>/pdf-helper/SKILL.md` and `<base>/pdf-helper/references/forms.md`.
fn pdf_helper_folder(base: &Path) -> PathBuf {
    let folder = skill_folder(base, "pdf-helper", PDF_HELPER_SKILL_MD);
    fs::create_dir_all(folder.join("references")).unwrap();
    fs::write(folder.join("references/forms.md"), "# Forms\n").unwrap();
    folder
}

/// A front block with `name`, `description` and `metadata.version: "1.0.0"`, then a body.
fn front_block(name: &str, description: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nmetadata:\n  version: \"1.0.0\"\n---\n# Body\n"
    )
}

/// Builds `folder` from an empty cwd and asserts it is refused with `E-BLD-009`, stderr holding
/// every one of `expected`, and that neither the cwd nor the folder gains a zip or a `.tmp`.
fn assert_e_bld_009(folder: &Path, expected: &[&str]) {
    let cwd = tempdir().unwrap();
    let before = files_in(folder);
    let assert = mur_build(cwd.path(), &[folder.to_str().unwrap()]).failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("error[E-BLD-009]: "), "{stderr}");
    assert!(stderr.contains("\n  hint: "), "{stderr}");
    for text in expected {
        assert!(stderr.contains(text), "stderr lacks {text:?}:\n{stderr}");
    }
    assert!(
        files_in(cwd.path()).is_empty(),
        "{:?}",
        files_in(cwd.path())
    );
    assert_eq!(
        files_in(folder),
        before,
        "the refused build wrote into the folder"
    );
}

// ── S1: a standard skill folder builds and installs ──────────────────────────

#[test]
fn s1_skill_folder_builds_from_its_front_block_and_installs() {
    let base = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());

    mur_build(cwd.path(), &[folder.to_str().unwrap()])
        .success()
        .stdout(
            predicate::str::is_match(
                r"^Built artifact: .*/pdf-helper-1\.2\.0\.mur\.zip \(WIT level \d+\)\n$",
            )
            .unwrap(),
        );

    let zip = cwd.path().join(PDF_HELPER_ZIP);
    assert_eq!(files_in(cwd.path()), [PDF_HELPER_ZIP]);
    assert_eq!(zip_entries(&zip), PDF_HELPER_ENTRIES);
    assert_eq!(
        zip_entry_bytes(&zip, "SKILL.md"),
        PDF_HELPER_SKILL_MD.as_bytes()
    );
    assert_eq!(zip_entry_content(&zip, "references/forms.md"), "# Forms\n");
    let manifest_text = zip_entry_content(&zip, "murmur.yaml");
    assert!(
        manifest_text.contains("description: Fill PDF forms; use when a form must be filled.\n"),
        "{manifest_text}"
    );
    let manifest = manifest_yaml(&zip);
    assert_eq!(manifest["name"].as_str(), Some("pdf-helper"));
    assert_eq!(manifest["version"].as_str(), Some("1.2.0"));
    assert_eq!(manifest["runtime"].as_str(), Some("skill"));
    assert_eq!(files_in(&folder), ["SKILL.md", "references"]);

    let home = tempdir().unwrap();
    common::publish_local(&home, &zip)
        .success()
        .stdout(predicate::str::contains("Published local/pdf-helper@1.2.0"));
    let capsule_dir = tempdir().unwrap();
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        "name: test-capsule\nversion: 0.1.0\nartifacts:\n  - name: local/pdf-helper\n    version: 1.2.0\n    runtime: skill\n",
    )
    .unwrap();
    let staged = stage_capsule(&home, capsule_dir.path(), &manifest_path);

    let tool_dir = staged.runtime_dir.join("tools").join("pdf-helper");
    assert_eq!(
        files_in(&tool_dir),
        ["SKILL.md", "murmur.yaml", "references"]
    );
    assert_eq!(
        fs::read(tool_dir.join("SKILL.md")).unwrap(),
        PDF_HELPER_SKILL_MD.as_bytes()
    );
    assert_eq!(
        fs::read_to_string(tool_dir.join("references/forms.md")).unwrap(),
        "# Forms\n"
    );
    assert_eq!(files_in(&tool_dir.join("references")), ["forms.md"]);
    let installed: serde_norway::Value =
        serde_norway::from_str(&fs::read_to_string(tool_dir.join("murmur.yaml")).unwrap()).unwrap();
    assert_eq!(installed["name"].as_str(), Some("pdf-helper"));
}

// ── S2: the file is named SKILL.md, exactly ───────────────────────────────────

#[test]
fn s2_a_misspelled_skill_md_is_refused_with_the_rename() {
    for spelling in ["skill.md", "Skill.md"] {
        let base = tempdir().unwrap();
        let folder = base.path().join("pdf-helper");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(spelling), PDF_HELPER_SKILL_MD).unwrap();

        assert_e_bld_009(
            &folder,
            &[
                &format!(
                    "{}: a skill's file is named SKILL.md, exactly",
                    folder.join(spelling).display()
                ),
                &format!("hint: rename {spelling} to SKILL.md"),
            ],
        );
    }
}

#[test]
fn s2_skill_md_beside_a_lowercase_spelling_is_refused_naming_the_lowercase_one() {
    let base = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::write(folder.join("skill.md"), "# other\n").unwrap();

    assert_e_bld_009(
        &folder,
        &[
            &format!(
                "{}: a skill's file is named SKILL.md, exactly",
                folder.join("skill.md").display()
            ),
            "hint: rename skill.md to SKILL.md",
        ],
    );
}

// ── S3: the front block follows the standard ──────────────────────────────────

#[test]
fn s3_front_block_rules_are_refused_naming_the_rule() {
    let long_description = "d".repeat(1025);
    let cases: Vec<(&str, String, String)> = vec![
        (
            "pdf-helper",
            "# PDF helper\nNo front block.\n".to_string(),
            "has no front block: the file must open with a line '---'".to_string(),
        ),
        (
            "pdf-helper",
            front_block("pdf-helper", &long_description),
            "front block description is 1025 characters; a description is at most 1024"
                .to_string(),
        ),
        (
            "PDF-helper",
            front_block("PDF-helper", "Fill PDF forms."),
            "front block name 'PDF-helper' may contain only lowercase letters, digits and '-'"
                .to_string(),
        ),
        (
            "pdf-helper",
            front_block("other-name", "Fill PDF forms."),
            "front block name 'other-name' differs from the folder name 'pdf-helper'".to_string(),
        ),
        (
            "pdf-helper",
            "---\nname: pdf-helper\ndescription: Fill PDF forms.\n---\n# Body\n".to_string(),
            "front block has no metadata.version".to_string(),
        ),
        (
            "pdf-helper",
            "---\nname: pdf-helper\ndescription: Fill PDF forms.\nmetadata:\n  version: \"1.2\"\n---\n# Body\n"
                .to_string(),
            "front block metadata.version '1.2' is not a semver version: ".to_string(),
        ),
        (
            "pdf-helper",
            front_block("pdf-helper", "Fill: forms"),
            "front block is not valid YAML: ".to_string(),
        ),
    ];
    for (folder_name, skill_md, rule) in cases {
        let base = tempdir().unwrap();
        let folder = skill_folder(base.path(), folder_name, &skill_md);
        let message = format!("{}: {rule}", folder.join("SKILL.md").display());
        assert_e_bld_009(&folder, &[&message]);
    }
}

#[test]
fn s3_a_front_block_that_is_not_yaml_names_the_quoting_fix() {
    let base = tempdir().unwrap();
    let folder = skill_folder(
        base.path(),
        "pdf-helper",
        &front_block("pdf-helper", "Fill: forms"),
    );
    assert_e_bld_009(&folder, &["quote a value that holds ': '"]);
}

#[test]
fn s3_limits_are_inclusive() {
    let base = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let longest_name = "a".repeat(64);
    let cases = [
        ("pdf-helper".to_string(), "d".repeat(1024)),
        (longest_name.clone(), "Fill PDF forms.".to_string()),
    ];
    for (name, description) in cases {
        let folder = skill_folder(base.path(), &name, &front_block(&name, &description));
        mur_build(cwd.path(), &[folder.to_str().unwrap()]).success();
        let zip = cwd.path().join(format!("{name}-1.0.0.mur.zip"));
        let manifest = manifest_yaml(&zip);
        assert_eq!(manifest["name"].as_str(), Some(name.as_str()));
        assert_eq!(manifest["description"].as_str(), Some(description.as_str()));
    }
}

// ── S4: no murmur.yaml beside SKILL.md, no flags that override the front block ─

#[test]
fn s4_a_murmur_yaml_beside_skill_md_is_refused() {
    for spelling in ["murmur.yaml", "Murmur.YAML"] {
        let base = tempdir().unwrap();
        let folder = pdf_helper_folder(base.path());
        fs::write(
            folder.join(spelling),
            "name: pdf-helper\nversion: 9.9.9\nruntime: skill\n",
        )
        .unwrap();

        assert_e_bld_009(
            &folder,
            &[
                &format!(
                    "{}: a skill folder holds no murmur.yaml beside SKILL.md",
                    folder.join(spelling).display()
                ),
                "hint: a skill's name, description and version go in the SKILL.md front block; \
                 delete murmur.yaml",
            ],
        );
    }
}

#[test]
fn s4_removed_flags_are_unexpected_arguments() {
    let base = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    let folder = folder.to_str().unwrap();

    let flag_sets: [&[&str]; 4] = [
        &["--skill"],
        &["--skill", "x"],
        &["--summary", "x"],
        &["--version", "1.0.0"],
    ];
    for flags in flag_sets {
        let mut args = vec![folder];
        args.extend_from_slice(flags);
        mur_build(cwd.path(), &args)
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
        assert!(files_in(cwd.path()).is_empty(), "{flags:?}");
    }
}

// ── S7: every other standard front-block field passes through untouched ──────

#[test]
fn s7_optional_standard_fields_build_and_install_byte_for_byte() {
    let skill_md = "---\nname: pdf-helper\ndescription: Fill PDF forms; use when a form must be filled.\nlicense: MIT\ncompatibility: Requires python3\nallowed-tools: Bash(python3:*) Read\nmetadata:\n  version: \"1.2.0\"\n  author: someone\nwhen_to_use: When a PDF form arrives.\ndisable-model-invocation: false\n---\n# PDF helper\n";
    let base = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let folder = skill_folder(base.path(), "pdf-helper", skill_md);

    mur_build(cwd.path(), &[folder.to_str().unwrap()]).success();
    let zip = cwd.path().join(PDF_HELPER_ZIP);
    assert_eq!(zip_entry_bytes(&zip, "SKILL.md"), skill_md.as_bytes());

    let home = tempdir().unwrap();
    common::publish_local(&home, &zip).success();
    let capsule_dir = tempdir().unwrap();
    let manifest_path = capsule_dir.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        "name: test-capsule\nversion: 0.1.0\nartifacts:\n  - name: local/pdf-helper\n    version: 1.2.0\n    runtime: skill\n",
    )
    .unwrap();
    let staged = stage_capsule(&home, capsule_dir.path(), &manifest_path);
    assert_eq!(
        fs::read(staged.runtime_dir.join("tools/pdf-helper/SKILL.md")).unwrap(),
        skill_md.as_bytes()
    );
}

// ── What the folder walk packs, and where the zip goes ────────────────────────

#[test]
fn folder_without_manifest_or_skill_md_fails_as_a_missing_manifest() {
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
fn skill_build_skips_dot_entries_and_built_artifacts() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::create_dir_all(folder.join(".git")).unwrap();
    fs::write(folder.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(folder.join(".DS_Store"), "x").unwrap();
    fs::write(folder.join("old-0.0.1.mur.zip"), "stale").unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    assert_eq!(
        zip_entries(&outdir.path().join(PDF_HELPER_ZIP)),
        PDF_HELPER_ENTRIES
    );
}

#[test]
fn skill_build_packs_supporting_files_recursively() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::create_dir_all(folder.join("scripts/lib")).unwrap();
    fs::write(folder.join("scripts/lib/fill.py"), "print(1)\n").unwrap();

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    assert_eq!(
        zip_entries(&outdir.path().join(PDF_HELPER_ZIP)),
        [
            "SKILL.md",
            "murmur-build.json",
            "murmur.yaml",
            "references/forms.md",
            "scripts/lib/fill.py"
        ]
    );
}

#[test]
fn front_block_folded_description_is_written_as_yaml() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = skill_folder(
        base.path(),
        "folded",
        "---\nname: folded\ndescription: >\n  Quotes ' and \" and: colons\n  on two lines.\nmetadata:\n  version: \"0.3.0\"\n---\n# F\n",
    );

    mur_build(outdir.path(), &[folder.to_str().unwrap()]).success();

    let zip = outdir.path().join("folded-0.3.0.mur.zip");
    let manifest_text = zip_entry_content(&zip, "murmur.yaml");
    assert!(
        manifest_text.starts_with("name: folded\nversion: '0.3.0'\nruntime: skill\ndescription: "),
        "got: {manifest_text}"
    );
    assert_eq!(
        manifest_yaml(&zip)["description"].as_str(),
        Some("Quotes ' and \" and: colons on two lines.")
    );
}

/// Builds `folder` and asserts it is refused with `E-BLD-002` naming `names`, with no zip in
/// `outdir` and no zip or `.tmp` left in the folder.
fn assert_e_bld_002_with_no_output(folder: &Path, outdir: &Path, names: &str) {
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
fn skill_build_refuses_a_symlink() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    std::os::unix::fs::symlink("forms.md", folder.join("references/link.md")).unwrap();

    assert_e_bld_002_with_no_output(&folder, outdir.path(), "references/link.md");
}

#[test]
fn skill_build_refuses_an_authored_build_record() {
    let base = tempdir().unwrap();
    let outdir = tempdir().unwrap();
    let folder = pdf_helper_folder(base.path());
    fs::write(folder.join("murmur-build.json"), "{\"built_at_level\":9}").unwrap();

    assert_e_bld_002_with_no_output(&folder, outdir.path(), "murmur-build.json");
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
    assert_eq!(files_in(&dist), [PDF_HELPER_ZIP]);

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
    assert_eq!(zip_entries(&inside), PDF_HELPER_ENTRIES);
    assert!(files_in(cwd.path()).is_empty());
    assert!(
        !files_in(&folder).iter().any(|name| name.ends_with(".tmp")),
        "{:?}",
        files_in(&folder)
    );
}

// ── S8: an archive built before skills named their file SKILL.md ──────────────

/// The store directory `home`'s `mur install` and `mur publish` write under.
fn store_dir(home: &TempDir) -> PathBuf {
    home.path().join(".murmur").join("artifacts")
}

fn assert_old_layout_refusal(stderr: &str, verb: &str) {
    assert!(
        stderr.contains(&format!(
            "error[E-REG-017]: skill artifact local/old-skill@0.1.0 cannot be {verb}: it holds \
             skill.md, so it was built before skills followed the Agent Skills standard, which \
             names the file SKILL.md"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "hint: install a newer version, built with mur build from a folder holding SKILL.md"
        ),
        "{stderr}"
    );
}

#[test]
fn s8_install_and_publish_refuse_an_old_layout_skill() {
    let artifacts = tempdir().unwrap();
    let zip =
        common::create_old_layout_skill_artifact(artifacts.path(), "old-skill", "0.1.0", "# Old\n");

    let home = tempdir().unwrap();
    let publish = common::publish_local(&home, &zip).failure();
    assert_old_layout_refusal(
        &String::from_utf8_lossy(&publish.get_output().stderr),
        "stored",
    );
    assert!(
        !store_dir(&home).join("local").join("old-skill").exists(),
        "a refused publish stored the skill"
    );

    let home = tempdir().unwrap();
    let install = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(home.path())
        .args(["install", "-g", zip.to_str().unwrap()])
        .assert()
        .failure();
    assert_old_layout_refusal(
        &String::from_utf8_lossy(&install.get_output().stderr),
        "stored",
    );
    assert!(
        !store_dir(&home).join("local").join("old-skill").exists(),
        "a refused install stored the skill"
    );
}

/// Every path under `dir` whose components end in `tools/old-skill`.
fn old_skill_tool_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                if path.ends_with("tools/old-skill") {
                    found.push(path.clone());
                }
                pending.push(path);
            }
        }
    }
    found
}

#[test]
fn s8_a_launch_refuses_an_old_layout_skill_already_in_the_store() {
    let home = tempdir().unwrap();
    let artifacts = tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        "dummy-driver",
        "0.1.0",
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    // An older mur stored the old layout with its `.sha256` beside it and pinned it in
    // murmur.lock: publish a current skill under the same name, then put the old bytes in its
    // place.
    let current = common::create_skill_artifact(artifacts.path(), "old-skill", "0.1.0", "# Old\n");
    common::publish_local(&home, &current).success();
    let old_dir = tempdir().unwrap();
    let old =
        common::create_old_layout_skill_artifact(old_dir.path(), "old-skill", "0.1.0", "# Old\n");
    common::overwrite_stored_payload(&home, "local/old-skill", "0.1.0", &old);

    let project = tempdir().unwrap();
    fs::write(
        project.path().join("murmur.lock"),
        format!(
            "lock_version: 3\nartifacts:\n- name: local/dummy-driver\n  resolved_version: 0.1.0\n  sha256:\n    any: {}\n  origin: operator\n- name: local/old-skill\n  resolved_version: 0.1.0\n  sha256:\n    any: {}\n  origin: operator\n",
            murmur_artifact::sha256_hex(&fs::read(&driver).unwrap()),
            murmur_artifact::sha256_hex(&fs::read(&old).unwrap()),
        ),
    )
    .unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        "name: cap\nversion: 0.1.0\ninference:\n  transport: http\n  model: test\n  driver:\n    artifact: local/dummy-driver\nartifacts:\n  - name: local/old-skill\n    version: 0.1.0\n    runtime: skill\n  - name: local/dummy-driver\n    version: 0.1.0\n    runtime: driver\n    gateway:\n      endpoint: http://localhost:9999\n      keyless: true\n",
    )
    .unwrap();

    let run = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&run.get_output().stderr).into_owned();
    assert!(
        stderr.contains(
            "error[E-REG-017]: skill artifact local/old-skill@0.1.0 cannot be installed: it \
             holds skill.md, so it was built before"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "hint: install a newer version, built with mur build from a folder holding SKILL.md"
        ),
        "{stderr}"
    );
    assert_eq!(old_skill_tool_dirs(home.path()), Vec::<PathBuf>::new());
    assert_eq!(old_skill_tool_dirs(project.path()), Vec::<PathBuf>::new());
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
