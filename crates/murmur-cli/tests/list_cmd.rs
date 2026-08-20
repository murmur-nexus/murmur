use std::{io::Write, path::Path};

use assert_cmd::Command;
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeType};
use predicates::prelude::*;
use tempfile::TempDir;

// ── Registry helpers ──────────────────────────────────────────────────────────

fn global_registry_root(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".murmur/artifacts")
}

fn project_registry_root(project_dir: &Path) -> std::path::PathBuf {
    project_dir.join(".murmur/artifacts")
}

fn publish_to_global(home: &TempDir, meta: ArtifactMeta) {
    let reg = LocalRegistry::new(global_registry_root(home));
    reg.publish(meta, b"fake-artifact-bytes").unwrap();
}

fn publish_to_project(project_dir: &Path, meta: ArtifactMeta) {
    let reg = LocalRegistry::new(project_registry_root(project_dir));
    reg.publish(meta, b"fake-artifact-bytes").unwrap();
}

fn artifact_meta(
    name: &str,
    version: &str,
    runtime: RuntimeType,
    platforms: Vec<(&str, &str)>,
) -> ArtifactMeta {
    ArtifactMeta {
        name: name.to_string(),
        version: version.to_string(),
        artifact_runtime: runtime.as_str().to_string(),
        runtime,
        platforms: platforms
            .into_iter()
            .map(|(os, arch)| (os.to_string(), arch.to_string()))
            .collect(),
        description: None,
        tags: vec![],
    }
}

/// Create a minimal murmur.yaml so find_project_root() recognises the dir.
fn create_project_manifest(dir: &Path) {
    let mut f = std::fs::File::create(dir.join("murmur.yaml")).unwrap();
    writeln!(f, "name: test-capsule").unwrap();
    writeln!(f, "version: 0.1.0").unwrap();
}

// ── Command runner ────────────────────────────────────────────────────────────

/// Run `mur list` with HOME set and the given extra args, from the workspace
/// default CWD (no explicit current_dir — used for global-store tests via -g).
fn mur_list(home: &TempDir, extra_args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).arg("list");
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.assert()
}

/// Run `mur list` from a specific project directory.
fn mur_list_in(
    home: &TempDir,
    project_dir: &Path,
    extra_args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .current_dir(project_dir)
        .arg("list");
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.assert()
}

// ── Tests: global store (-g) ──────────────────────────────────────────────────

#[test]
fn list_empty_global_registry_prints_no_artifacts() {
    let home = tempfile::tempdir().unwrap();
    mur_list(&home, &["-g"])
        .success()
        .stdout(predicate::str::contains("No artifacts found."));
}

#[test]
fn list_global_shows_header_and_artifact_row() {
    let home = tempfile::tempdir().unwrap();
    publish_to_global(
        &home,
        artifact_meta(
            "my-tool",
            "1.2.3",
            RuntimeType::Wasm,
            vec![("linux", "amd64")],
        ),
    );

    mur_list(&home, &["-g"])
        .success()
        .stdout(predicate::str::contains("NAME"))
        .stdout(predicate::str::contains("VERSION"))
        .stdout(predicate::str::contains("RUNTIME"))
        .stdout(predicate::str::contains("PLATFORMS"))
        .stdout(predicate::str::contains("my-tool"))
        .stdout(predicate::str::contains("1.2.3"))
        .stdout(predicate::str::contains("wasm"))
        .stdout(predicate::str::contains("linux-amd64"));
}

#[test]
fn list_global_shows_multiple_artifacts_sorted_by_name_then_version() {
    let home = tempfile::tempdir().unwrap();
    publish_to_global(
        &home,
        artifact_meta("zeta-tool", "0.1.0", RuntimeType::Wasm, vec![]),
    );
    publish_to_global(
        &home,
        artifact_meta(
            "alpha-tool",
            "2.0.0",
            RuntimeType::Native,
            vec![("darwin", "arm64")],
        ),
    );

    let output = mur_list(&home, &["-g"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    let alpha_pos = text.find("alpha-tool").unwrap();
    let zeta_pos = text.find("zeta-tool").unwrap();
    assert!(
        alpha_pos < zeta_pos,
        "expected alpha-tool before zeta-tool in output"
    );
}

#[test]
fn list_global_formats_multiple_platforms_comma_separated() {
    let home = tempfile::tempdir().unwrap();
    publish_to_global(
        &home,
        artifact_meta(
            "multi-plat",
            "0.3.0",
            RuntimeType::Native,
            vec![("darwin", "arm64"), ("linux", "amd64"), ("linux", "arm64")],
        ),
    );

    mur_list(&home, &["-g"])
        .success()
        .stdout(predicate::str::contains(
            "darwin-arm64, linux-amd64, linux-arm64",
        ));
}

#[test]
fn list_global_flag_shows_global_store() {
    let home = tempfile::tempdir().unwrap();
    publish_to_global(
        &home,
        artifact_meta("flag-tool", "0.1.0", RuntimeType::Wasm, vec![]),
    );

    mur_list(&home, &["-g"])
        .success()
        .stdout(predicate::str::contains("flag-tool"));
}

// ── Tests: project store (default in project dir) ────────────────────────────

#[test]
fn list_defaults_to_project_store_in_project_dir() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project_manifest(project.path());

    publish_to_project(
        project.path(),
        artifact_meta("proj-tool", "1.0.0", RuntimeType::Wasm, vec![]),
    );
    // Also publish a different artifact to global — it should NOT appear.
    publish_to_global(
        &home,
        artifact_meta("global-only", "9.0.0", RuntimeType::Wasm, vec![]),
    );

    mur_list_in(&home, project.path(), &[])
        .success()
        .stdout(predicate::str::contains("proj-tool"))
        .stdout(predicate::str::contains("1.0.0"))
        .stdout(predicate::str::contains("global-only").not());
}

#[test]
fn list_global_flag_in_project_dir_shows_global_not_project() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project_manifest(project.path());

    publish_to_project(
        project.path(),
        artifact_meta("proj-tool", "1.0.0", RuntimeType::Wasm, vec![]),
    );
    publish_to_global(
        &home,
        artifact_meta("global-only", "9.0.0", RuntimeType::Wasm, vec![]),
    );

    mur_list_in(&home, project.path(), &["-g"])
        .success()
        .stdout(predicate::str::contains("global-only"))
        .stdout(predicate::str::contains("proj-tool").not());
}

#[test]
fn list_outside_project_dir_falls_back_to_global_store() {
    let home = tempfile::tempdir().unwrap();
    // No murmur.yaml in home dir → not a project
    publish_to_global(
        &home,
        artifact_meta("global-tool", "2.0.0", RuntimeType::Wasm, vec![]),
    );

    // Run from home.path() which has no murmur.yaml
    mur_list_in(&home, home.path(), &[])
        .success()
        .stdout(predicate::str::contains("global-tool"));
}

#[test]
fn list_empty_project_store_prints_no_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project_manifest(project.path());
    // No artifacts published to project store

    mur_list_in(&home, project.path(), &[])
        .success()
        .stdout(predicate::str::contains("No artifacts found."));
}

// ── Tests: --all ─────────────────────────────────────────────────────────────

#[test]
fn list_all_shows_scope_column_with_both_stores() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project_manifest(project.path());

    publish_to_project(
        project.path(),
        artifact_meta("proj-tool", "1.0.0", RuntimeType::Wasm, vec![]),
    );
    publish_to_global(
        &home,
        artifact_meta("global-tool", "2.0.0", RuntimeType::Wasm, vec![]),
    );

    mur_list_in(&home, project.path(), &["--all"])
        .success()
        .stdout(predicate::str::contains("SCOPE"))
        .stdout(predicate::str::contains("project"))
        .stdout(predicate::str::contains("global"))
        .stdout(predicate::str::contains("proj-tool"))
        .stdout(predicate::str::contains("global-tool"));
}

#[test]
fn list_all_project_appears_before_global() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project_manifest(project.path());

    publish_to_project(
        project.path(),
        artifact_meta("proj-tool", "1.0.0", RuntimeType::Wasm, vec![]),
    );
    publish_to_global(
        &home,
        artifact_meta("global-tool", "2.0.0", RuntimeType::Wasm, vec![]),
    );

    let output = mur_list_in(&home, project.path(), &["--all"])
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();

    let proj_pos = text.find("proj-tool").unwrap();
    let global_pos = text.find("global-tool").unwrap();
    assert!(
        proj_pos < global_pos,
        "expected project artifact before global artifact"
    );
}

#[test]
fn list_all_conflicts_with_global_flag() {
    let home = tempfile::tempdir().unwrap();
    mur_list(&home, &["--all", "-g"]).failure();
}
