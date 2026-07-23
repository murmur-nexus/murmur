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

    // The fixture also carries a README.md, which the manifest does not declare. A built
    // artifact ships what `requires_files:` names and nothing else, so it stays behind.
    assert_eq!(
        names,
        vec!["murmur.yaml".to_string(), "tool.wasm".to_string()]
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
        format!(
            "name: secret-demo\nversion: 0.0.1\nruntime: wasm\napi_key: {key}\nrequires_files:\n  - tool.wasm\n"
        ),
    )
    .unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();

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

/// A wasm artifact whose payload the runtime could not select is a build failure, with the
/// message the runtime itself would have printed at launch — and nothing left on disk.
#[test]
fn two_root_wasm_files_fail_the_build_with_no_artifact_written() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: ambiguous-tool\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - alpha.wasm\n  - zeta.wasm\n",
    )
    .unwrap();
    fs::write(dir.path().join("alpha.wasm"), b"\0asm").unwrap();
    fs::write(dir.path().join("zeta.wasm"), b"\0asm").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error[E-BLD-003]:").and(predicate::str::contains(
                "multiple root .wasm files found: alpha.wasm, zeta.wasm",
            )),
        );

    assert!(!dir.path().join("ambiguous-tool-0.1.0.mur.zip").exists());
}

#[test]
fn an_invalid_artifact_name_fails_the_build() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: My Tool\nversion: 0.1.0\nruntime: skill\nrequires_files: []\n",
    )
    .unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error[E-BLD-001]:")
                .and(predicate::str::contains("invalid artifact name 'My Tool'")),
        );
}

#[test]
fn a_declared_cargo_toml_warns_but_the_build_still_succeeds() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: sources-shipped\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n  - Cargo.toml\n",
    )
    .unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("warning[W-BLD-003]:")
                .and(predicate::str::contains("Cargo.toml")),
        );

    assert!(dir.path().join("sources-shipped-0.1.0.mur.zip").exists());
}

/// The lints are silent on a well-formed artifact: the in-workspace fixture builds with an
/// empty stderr, so a warning line means something actually changed.
#[test]
fn the_happy_fixture_builds_without_any_warning() {
    let fixture = fixture_path("happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
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
