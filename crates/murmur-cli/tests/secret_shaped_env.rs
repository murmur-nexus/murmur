//! `W-SEC-024`: what an operator sees when `capabilities.env.allow` names a credential-shaped
//! variable.
//!
//! Driven through the real `mur` binary because the claim is about two stderr streams reading
//! identically, and about which names are reported at all — an in-process assertion about the
//! decision cannot tell you whether `mur run` and `mur doctor` print the same bytes, or whether
//! the emitter is reached at all before `--explain-scope` returns. A name the credential backstop
//! strips is refused with `E-CAP-016` instead; `env_allow_stripped.rs` covers that.

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

const CODE: &str = "W-SEC-024";
const LINK: &str = "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-024";

fn project(dir: &Path, manifest: &str) {
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

/// `--explain-scope` returns ahead of every side effect, so these need no installed artifact and
/// no registry.
fn explain_scope_stderr(home: &TempDir, project_dir: &Path, extra_args: &[&str]) -> String {
    explain_scope_stderr_with_env(home, project_dir, extra_args, &[])
}

fn explain_scope_stderr_with_env(
    home: &TempDir,
    project_dir: &Path,
    extra_args: &[&str],
    env: &[(&str, &str)],
) -> String {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
        .args(extra_args);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.assert().success().get_output().stderr.clone();
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

/// One credential-shaped name the backstop's fixed list does not cover, under the default
/// lifecycle: the capsule holds the host's value for as long as it runs, and the line says so
/// without reading that value.
#[test]
fn a_held_secret_is_named_once_with_its_doc_link() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - DATABASE_PASSWORD\n",
    );

    // The host holds the variable, so a line that echoed the value would be caught below.
    let stderr = explain_scope_stderr_with_env(
        &home,
        dir.path(),
        &[],
        &[("DATABASE_PASSWORD", "hunter2hunter2")],
    );
    let lines = warning_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    assert!(lines[0].contains("'DATABASE_PASSWORD'"), "{}", lines[0]);
    assert!(lines[0].contains("the capsule holds it"), "{}", lines[0]);
    assert!(lines[0].contains(LINK), "{}", lines[0]);
    // The judgment is made from the name alone, so nothing the host holds can reach the line.
    assert!(!stderr.contains("hunter2hunter2"), "stderr was: {stderr}");
}

/// A capsule that keeps holding the value after the task that launched it is gone gets one further
/// sentence in the same line.
#[test]
fn after_task_sleep_adds_the_outliving_clause() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\nlifecycle:\n  after_task: sleep\ncapabilities:\n  env:\n    \
         allow:\n      - DATABASE_PASSWORD\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);
    let lines = warning_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    assert!(
        lines[0].contains("lifecycle.after_task: sleep"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("with nothing left waiting on it"),
        "{}",
        lines[0]
    );
}

/// `--lifecycle-after-task sleep` on a manifest that declares no `lifecycle` block reaches the same
/// verdict as declaring it, because the emitter resolves the override the launch would resolve.
#[test]
fn the_cli_lifecycle_override_is_honoured() {
    let home = TempDir::new().unwrap();
    let declared = TempDir::new().unwrap();
    project(
        declared.path(),
        "name: demo\nversion: 0.1.0\nlifecycle:\n  after_task: sleep\ncapabilities:\n  env:\n    \
         allow:\n      - DATABASE_PASSWORD\n",
    );
    let overridden = TempDir::new().unwrap();
    project(
        overridden.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - DATABASE_PASSWORD\n",
    );

    let from_manifest = explain_scope_stderr(&home, declared.path(), &[]);
    let from_flag = explain_scope_stderr(
        &home,
        overridden.path(),
        &["--lifecycle-after-task", "sleep"],
    );

    assert_eq!(
        warning_lines(&from_flag),
        warning_lines(&from_manifest),
        "the flag and the manifest block must reach the same line"
    );
    assert_eq!(warning_lines(&from_flag).len(), 1);
}

/// Names only the segment rule catches reach every guest and are each reported, in declaration
/// order, in the same bytes from `mur run --explain-scope` and `mur doctor`.
#[test]
fn segment_matched_names_are_each_reported_in_order_by_run_and_doctor() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - PRIVATE_KEY\n      - SSH_KEY\n      - CREDENTIALS\n      - SENTRY_DSN\n",
    );

    let from_run = explain_scope_stderr(&home, dir.path(), &[]);
    let from_doctor = doctor_stderr(&home, dir.path());
    let run_lines = warning_lines(&from_run);

    assert_eq!(run_lines.len(), 4, "stderr was: {from_run}");
    for (line, name) in
        run_lines
            .iter()
            .zip(["PRIVATE_KEY", "SSH_KEY", "CREDENTIALS", "SENTRY_DSN"])
    {
        assert!(line.contains(&format!("'{name}'")), "{line}");
        assert!(line.contains("the capsule holds it"), "{line}");
    }
    assert_eq!(run_lines, warning_lines(&from_doctor));
}

/// Ordinary names that the segment rule is written to pass over — a substring of a marker, or the
/// shell's working directory — produce neither the warning nor the refusal.
#[test]
fn near_miss_ordinary_names_are_silent() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - TZ\n      - AUTHOR_NAME\n      - KEYBOARD_LAYOUT\n      - PWD\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);

    assert!(!stderr.contains(CODE), "stderr was: {stderr}");
    assert!(!stderr.contains("E-CAP-016"), "stderr was: {stderr}");
}

/// A segment-matched name on a sleeping capsule gets the outliving clause, once.
#[test]
fn a_segment_matched_name_under_sleep_gets_the_outliving_clause() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\nlifecycle:\n  after_task: sleep\ncapabilities:\n  env:\n    \
         allow:\n      - SIGNING_KEY\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);
    let lines = warning_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    assert!(lines[0].contains("'SIGNING_KEY'"), "{}", lines[0]);
    assert!(
        lines[0].contains("lifecycle.after_task: sleep"),
        "{}",
        lines[0]
    );
}

/// Ordinary environment names are not secrets, and `after_task: sleep` on its own is not a trigger:
/// there is nothing being held for the capsule to outlive its launcher with.
#[test]
fn non_credential_names_warn_about_nothing() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\nlifecycle:\n  after_task: sleep\ncapabilities:\n  env:\n    \
         allow:\n      - HOME\n      - TZ\n      - LANG\n      - BUILD_NUMBER\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);

    assert!(!stderr.contains(CODE), "stderr was: {stderr}");
}

/// A manifest with no `capabilities` block at all declares no grant to judge.
#[test]
fn a_manifest_without_capabilities_is_silent() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), "name: demo\nversion: 0.1.0\n");

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);

    assert!(!stderr.contains(CODE), "stderr was: {stderr}");
}

/// One emitter, two call sites: an operator who reads the grant from `mur doctor` reads the same
/// bytes a launch would have printed.
#[test]
fn run_and_doctor_print_the_same_line() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - DATABASE_PASSWORD\n",
    );

    let from_run = explain_scope_stderr(&home, dir.path(), &[]);
    let from_doctor = doctor_stderr(&home, dir.path());

    let run_lines = warning_lines(&from_run);
    let doctor_lines = warning_lines(&from_doctor);
    assert_eq!(run_lines.len(), 1, "run stderr was: {from_run}");
    assert_eq!(doctor_lines.len(), 1, "doctor stderr was: {from_doctor}");
    assert_eq!(run_lines[0], doctor_lines[0]);
}

/// One line per distinct credential-shaped name, in declaration order, with the repeat judged once
/// and the ordinary name passed over.
#[test]
fn every_distinct_name_is_reported_once_in_declaration_order() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - DATABASE_PASSWORD\n      - HOME\n      - SIGNING_KEY\n      - DATABASE_PASSWORD\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path(), &[]);
    let lines = warning_lines(&stderr);

    assert_eq!(lines.len(), 2, "stderr was: {stderr}");
    assert!(lines[0].contains("'DATABASE_PASSWORD'"), "{}", lines[0]);
    assert!(lines[1].contains("'SIGNING_KEY'"), "{}", lines[1]);
}
