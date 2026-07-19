use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;
use zip::ZipArchive;

#[test]
fn build_valid_fixture_creates_mur_zip_with_expected_layout() {
    let fixture = fixture_path("happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    let artifact = dir.path().join("out.mur.zip");

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            dir.path().to_str().unwrap(),
            "--output",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    assert!(artifact.exists());

    let file = fs::File::open(&artifact).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();

    let mut names = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).unwrap();
        names.push(file.name().to_string());
    }
    names.sort();

    assert_eq!(
        names,
        vec![
            "README.md".to_string(),
            "murmur.yaml".to_string(),
            "tool.wasm".to_string(),
        ]
    );

    let mut manifest = String::new();
    archive
        .by_name("murmur.yaml")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    assert!(manifest.contains("name: hello-slice"));
}

#[test]
fn missing_name_is_non_zero_with_actionable_error() {
    let fixture = fixture_path("missing-name");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing required field 'name'"));
}

#[test]
fn missing_version_is_non_zero_with_actionable_error() {
    let fixture = fixture_path("missing-version");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing required field 'version'"));
}

#[test]
fn malformed_yaml_is_non_zero_with_line_info() {
    let fixture = fixture_path("malformed");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("line"));
}

#[test]
fn literal_api_key_warns_but_build_succeeds() {
    let dir = tempdir().unwrap();
    // Manifest written at test time with a runtime-assembled key so the repo
    // never contains a credential-shaped literal that secret scanners could flag.
    let key = ["sk-", "ant-", "abc123456789"].concat();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!("name: secret-demo\nversion: 0.0.1\nruntime: wasm\napi_key: {key}\n"),
    )
    .unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("appears to contain a literal secret value")
                .and(predicate::str::contains("api_key")),
        );
}

#[test]
fn skill_artifact_build_succeeds_with_skill_md() {
    let fixture = fixture_path("skill-happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    let artifact = dir.path().join("out.mur.zip");

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            dir.path().to_str().unwrap(),
            "--output",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    assert!(artifact.exists());

    let file = fs::File::open(&artifact).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();

    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();

    assert!(
        names.contains(&"murmur.yaml".to_string()),
        "murmur.yaml must be present in skill artifact zip"
    );
    assert!(
        names.contains(&"skill.md".to_string()),
        "skill.md must be present in skill artifact zip"
    );
}

#[test]
fn skill_artifact_build_fails_without_skill_md() {
    let fixture = fixture_path("skill-missing-skill-md");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("skill.md"));
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
