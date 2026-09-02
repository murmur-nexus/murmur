//! `W-SEC-019`: what an operator sees when `murmur.yaml` carries a key this build does not
//! recognize.
//!
//! Driven through the real `mur` binary on both surfaces, because the claim is about two stderr
//! streams reading identically — an in-process assertion about one message builder cannot tell you
//! whether `mur run` and `mur doctor` actually print the same bytes.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const CODE: &str = "W-SEC-019";

fn project(dir: &Path, manifest: &str) {
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

/// `--explain-scope` returns ahead of every side effect, so these need no installed artifact and
/// no registry.
fn explain_scope_stderr(home: &TempDir, project_dir: &Path) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

/// Doctor's exit status depends on the artifact checklist, which these fixtures deliberately leave
/// empty, so the stream is taken without asserting on it.
fn doctor_stderr(home: &TempDir, project_dir: &Path) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

fn warning_lines(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|line| line.contains(CODE)).collect()
}

/// The hyphen-for-underscore typo, named with the block that held it and with the key it should
/// have been — and the run still exits 0, because an unrecognized key refuses nothing.
#[test]
fn a_typo_is_named_with_its_path_and_the_nearest_key() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: typo-capsule\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    read-only:\n      \
         - tests\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("unrecognized key 'read-only' in capabilities.filesystem"));
    assert!(lines[0].contains("did you mean 'read_only'?"));
    assert!(lines[0].contains("spelling problem"));
}

/// The two surfaces print one identical line. Asserted on the bytes as well as structurally
/// (both call `warn_on_unknown_manifest_keys`), because that is what an operator comparing the two
/// actually reads.
#[test]
fn run_and_doctor_print_the_same_bytes() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: typo-capsule\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    read-only:\n      \
         - tests\n",
    );

    assert_eq!(
        warning_lines(&explain_scope_stderr(&home, dir.path())),
        warning_lines(&doctor_stderr(&home, dir.path()))
    );
}

/// A manifest of recognized keys is silent on both surfaces. A warning that fires on correct input
/// is noise, and noise is what every operator learns to ignore.
#[test]
fn a_correct_manifest_warns_on_neither_surface() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        r#"name: clean-capsule
version: 0.1.0
artifacts:
  - name: notes-tool
    version: 0.1.0
    runtime: tool
    capabilities:
      network:
        allow:
          - example.com:443
capabilities:
  filesystem:
    read_only:
      - tests
      - bench/fixtures
  shell:
    allow:
      - git
  network:
    allow:
      - example.com:443
"#,
    );

    assert!(warning_lines(&explain_scope_stderr(&home, dir.path())).is_empty());
    assert!(warning_lines(&doctor_stderr(&home, dir.path())).is_empty());
}

/// A key with no near neighbour is worded as one this build does not know rather than as a
/// misspelling: sending an operator to hunt for a typo in a correct manifest is the failure this
/// warning exists to stop.
#[test]
fn a_key_with_no_near_neighbour_is_not_called_a_typo() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: newer-capsule\nversion: 0.1.0\nquantum_teleport: true\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("unrecognized key 'quantum_teleport' at the top level"));
    assert!(lines[0].contains("may come from a newer mur"));
    assert!(!lines[0].contains("did you mean"));
    assert!(!lines[0].contains("spelling"));
}

/// A pin higher than the running binary names the cause directly, so nobody has to infer a stale
/// binary from unfamiliar key names — and the pre-existing pin-mismatch warning is untouched.
#[test]
fn a_higher_pin_adds_a_line_naming_both_versions_and_the_count() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: pin-capsule\nversion: 0.1.0\nmur_version: \"99.0.0\"\nquantum_teleport: true\n\
         capabilities:\n  filesystem:\n    read-only:\n      - tests\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 3, "{stderr}");
    assert!(lines[2].contains(&format!(
        "this manifest pins mur 99.0.0, you are running {}; 2 keys in it are not recognized by \
         this build",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(
        stderr.contains(&format!(
            "warning: manifest requires mur 99.0.0 but you are running mur {}",
            env!("CARGO_PKG_VERSION")
        )),
        "{stderr}"
    );
}

// ── A full launch, not just the diagnostic ────────────────────────────────────

const TOOL_NAME: &str = "echo-tool";
const TOOL_VERSION: &str = "0.1.0";

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

fn tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact_path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {TOOL_NAME}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(fixture_component("echo-tool.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    artifact_path
}

/// An unrecognized key changes no exit code and refuses nothing: the capsule stages, launches and
/// completes with the warning printed beside it. This is the invariant the whole design rests on —
/// `deny_unknown_fields` is set nowhere, so a manifest written for a newer `mur` still runs here.
#[test]
fn a_capsule_with_an_unrecognized_key_still_launches() {
    let home = TempDir::new().unwrap();
    let fixture = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();

    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: {TOOL_NAME}\n    version: \
             {TOOL_VERSION}\ncapabilities:\n  filesystem:\n    read-only:\n      - tests\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-allowlisted.wasm"),
        dir.path().join("capsule.wasm"),
    )
    .unwrap();
    common::install_artifact_to_project(dir.path(), &tool_artifact(fixture.path())).success();

    common::run_capsule(&home, &dir.path().join("murmur.yaml"))
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .stderr(predicate::str::contains(
            "unrecognized key 'read-only' in capabilities.filesystem",
        ));
}
