#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use murmur_artifact::{
    read_lockfile, write_lockfile_atomic, LockOrigin, LockedArtifact, LockedSha256,
};
use predicates::prelude::*;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

/// The tool name a script capsule calls the fixture by; [`TOOL_ARTIFACT`] is the full name its
/// `.mur.zip` installs under.
const TOOL_NAME: &str = "echo-tool";
const TOOL_ARTIFACT: &str = "local/echo-tool";
const TOOL_VERSION: &str = "0.1.0";

/// `--task` names where the task lands: the session's runtime directory, which the capsule
/// cannot change, and not the workdir.
#[test]
fn run_help_says_the_task_is_written_to_the_runtime_directory() {
    let assert = Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let help = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = help.find("--task <TASK>").expect("a --task entry");
    let rest = &help[start + "--task <TASK>".len()..];
    let entry = &rest[..rest.find(" --").unwrap_or(rest.len())];
    assert!(entry.contains("runtime directory"), "{entry}");
    assert!(entry.contains("$MURMUR_RUNTIME_DIR/task.md"), "{entry}");
    assert!(entry.contains("the workdir is left alone"), "{entry}");
    assert!(!entry.contains("capsule workdir"), "{entry}");
}

#[test]
fn run_round_trip_writes_lock_and_uses_existing_lock_on_second_run() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let first = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let first_stdout = String::from_utf8(first).unwrap();
    let first_workdir = common::parse_workdir_from_stdout(&first_stdout);
    assert_eq!(
        fs::read_to_string(first_workdir.join("out/result.txt")).unwrap(),
        "ok"
    );

    let lock_path = project.path().join("murmur.lock");
    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock
        .artifact_for(TOOL_ARTIFACT)
        .expect("lock entry for echo-tool");
    assert_eq!(entry.resolved_version, TOOL_VERSION);
    assert!(!entry.sha256.any.as_deref().unwrap_or_default().is_empty());

    // Change the manifest version to a non-existent one. The second run should still
    // succeed by honoring the existing lock pin.
    fs::write(
        &manifest_path,
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: {TOOL_ARTIFACT}\n    version: 9.9.9\n"
        ),
    )
    .unwrap();

    let second = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let second_stdout = String::from_utf8(second).unwrap();
    let second_workdir = common::parse_workdir_from_stdout(&second_stdout);
    assert_eq!(
        fs::read_to_string(second_workdir.join("out/result.txt")).unwrap(),
        "ok"
    );
}

#[test]
fn run_unlisted_tool_call_returns_error_to_capsule_without_panic() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-unlisted.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let stdout = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(stdout).unwrap());
    let output = fs::read_to_string(workdir.join("out/result.txt")).unwrap();
    assert!(output.contains("not declared in manifest allowlist"));
}

#[test]
fn run_missing_artifact_fails_before_launch_with_clear_error() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        "  - name: local/missing-tool\n    version: 0.0.1\n",
        None,
    );

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains(
            "missing artifacts: local/missing-tool@0.0.1",
        ))
        .stderr(predicate::str::contains("mur install"));

    assert!(!project.path().join("workdir").exists());
}

#[test]
fn run_fails_with_e_run_008_when_artifact_not_installed() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains(format!(
            "missing artifacts: {TOOL_ARTIFACT}@{TOOL_VERSION}"
        )))
        .stderr(predicate::str::contains("mur install"));
}

/// `mur run` does not auto-pull: installing is `mur install`'s job, and `mur run` only ever
/// checks. Even with a source configured for the artifact's scope, a missing artifact fails fast
/// with E-RUN-008 and no network request is made.
///
/// `MUR_GITHUB_API_BASE` points at a bound-but-never-served listener: nothing accepts, so any
/// regression that reintroduces a pull hangs on connect rather than silently passing. The
/// GitHub source's real request/response behavior is covered by `tests/source_resolution.rs`.
#[test]
fn run_does_not_auto_pull_missing_artifact_even_when_its_scope_has_a_source() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    write_registry_source_config(home.path(), "acme/artifacts");
    // A scope a source serves: a `local/` artifact is never asked of one, so naming one would
    // prove nothing about the sources.
    const SOURCED: &str = "acme/echo-tool";

    // Bound but never accepted: proves `mur run` issues no request against a source.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {SOURCED}\n    version: {TOOL_VERSION}\n"),
        None,
    );

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", &api_base)
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--verbose",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains(format!(
            "missing artifacts: {SOURCED}@{TOOL_VERSION}"
        )));

    let installed_zip = project.path().join(format!(
        ".murmur/artifacts/{SOURCED}/{TOOL_VERSION}/{TOOL_NAME}-{TOOL_VERSION}.mur.zip"
    ));
    assert!(
        !installed_zip.exists(),
        "mur run must not install artifacts; that is `mur install`'s job"
    );
}

#[test]
fn second_run_detects_tampered_artifact_with_integrity_error() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    common::run_capsule(&home, &manifest_path).success();

    let installed_zip = project.path().join(format!(
        ".murmur/artifacts/{TOOL_ARTIFACT}/{TOOL_VERSION}/{TOOL_NAME}-{TOOL_VERSION}.mur.zip"
    ));
    fs::write(installed_zip, b"tampered").unwrap();

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains(format!(
            "artifact integrity check failed for {TOOL_ARTIFACT}@{TOOL_VERSION}"
        )));
}

#[test]
fn run_disallowed_network_call_is_denied_without_panic() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-network-attempt.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        Some("  network:\n    allow:\n      - https://allowed.example.com\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "denied"
    );
}

#[test]
fn run_allowlisted_network_host_is_not_denied() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-network-attempt.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        Some("  network:\n    allow:\n      - https://blocked.example.com\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "allowed"
    );
}

/// Runs the env-echo fixture with `extra_env` set on the `mur` process and returns the
/// `NAME=present:value` / `NAME=absent` lines the guest observed.
fn run_env_echo(capabilities_yaml: Option<&str>, extra_env: &[(&str, &str)]) -> String {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-env-echo.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        capabilities_yaml,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule_with_env(&home, &manifest_path, extra_env)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    fs::read_to_string(workdir.join("out/result.txt")).unwrap()
}

#[test]
fn run_without_env_capabilities_hides_host_credential_var_from_guest() {
    let result = run_env_echo(None, &[("GITHUB_TOKEN", "leaked-token")]);

    assert!(
        result.contains("GITHUB_TOKEN=absent"),
        "guest observed host GITHUB_TOKEN: {result}"
    );
}

#[test]
fn run_with_env_allowlist_passes_declared_host_var_to_guest() {
    let result = run_env_echo(
        Some("  env:\n    allow:\n      - MURMUR_TEST_ALLOWED_VAR\n"),
        &[
            ("MURMUR_TEST_ALLOWED_VAR", "visible-value"),
            ("GITHUB_TOKEN", "leaked-token"),
        ],
    );

    assert!(
        result.contains("MURMUR_TEST_ALLOWED_VAR=present:visible-value"),
        "declared var missing from guest: {result}"
    );
    // The allowlist grants only the name it declares, not the rest of the host env.
    assert!(
        result.contains("GITHUB_TOKEN=absent"),
        "undeclared var leaked to guest: {result}"
    );
}

/// Declaring a name the credential backstop strips is refused before launch with `E-CAP-016`,
/// and the host's value appears nowhere in the refusal.
#[test]
fn run_with_env_allowlisted_credential_var_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = create_project(
        project.path(),
        "capsule-env-echo.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        Some("  env:\n    allow:\n      - GITHUB_TOKEN\n"),
    );

    let stderr = String::from_utf8(
        common::run_capsule_with_env(&home, &manifest_path, &[("GITHUB_TOKEN", "leaked-token")])
            .failure()
            .get_output()
            .stderr
            .clone(),
    )
    .unwrap();

    assert!(stderr.contains("E-CAP-016"), "stderr was: {stderr}");
    assert!(!stderr.contains("leaked-token"), "stderr was: {stderr}");
}

#[test]
fn run_without_network_capabilities_denies_outbound_http() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-network-attempt.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "denied"
    );
}

#[test]
fn run_filesystem_escape_attempt_fails_and_does_not_write_outside_workdir() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let artifact_path = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );

    let manifest_path = create_project(
        project.path(),
        "capsule-filesystem-escape.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        Some("  filesystem:\n    scope: .\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = common::parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "blocked"
    );
    assert!(!project.path().join("outside.txt").exists());
}

/// Sets up a manifest-only "agent capsule" project (murmur.yaml, no capsule.wasm
/// needed since `inference` is present) whose driver's `gateway.api_key` references an env
/// var, plus a workspace-root `.env` that sets that var. The `murmur.yaml` doubles as
/// the workspace-root marker that `.env` auto-loading keys on. `artifacts` is appended verbatim
/// to the `artifacts:` block after the driver's entry: a launch that would otherwise get past
/// staging needs an uninstalled artifact in it, so `mur run` fails fast with E-RUN-008 rather than starting an
/// HTTP server and blocking forever.
fn create_dotenv_project(project_dir: &Path, env_var: &str, artifacts: &str) -> PathBuf {
    fs::write(project_dir.join(".env"), format!("{env_var}=from-dotenv\n")).unwrap();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: local/dummy-driver\n    version: 0.1.0\n    runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:8080\n      api_key: ${{{env_var}}}{artifacts}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: local/dummy-driver\n"
        ),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

/// With `.env` skipped and no config entry, the credential is held nowhere, so staging refuses
/// before anything launches. The only artifact declared is the driver, and it is installed, so
/// nothing earlier stops the run.
#[test]
fn run_no_env_file_skips_dotenv_and_refuses_the_unresolved_credential() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        "dummy-driver",
        "0.1.0",
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let manifest_path = create_dotenv_project(project.path(), "CI_TEST_VAR", "");

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("CI_TEST_VAR")
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--no-env-file",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-MAN-003"))
        .stderr(predicate::str::contains("gateway.api_key"))
        .stderr(predicate::str::contains("${CI_TEST_VAR}"));
}

#[test]
fn run_without_no_env_file_flag_loads_dotenv_and_reads_the_credential_from_it() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_dotenv_project(
        project.path(),
        "CI_TEST_VAR",
        "\n  - name: local/missing-tool\n    version: 0.0.1",
    );

    // No --no-env-file: .env is auto-loaded, so CI_TEST_VAR is in the environment `mur run`
    // reads the credential from. `W-SEC-027` names it — it fires only for a `${NAME}` the
    // environment holds — and the run then stops at the uninstalled-artifact check (E-RUN-008),
    // never at the unresolved-credential refusal (E-MAN-003).
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("CI_TEST_VAR")
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("W-SEC-027"))
        .stderr(predicate::str::contains("environment variable CI_TEST_VAR"))
        .stderr(predicate::str::contains("E-MAN-003").not())
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains("local/missing-tool@0.0.1"));
}

/// A project declaring `echo-tool` with its artifact installed and no `murmur.lock` yet.
fn echo_tool_project(project: &Path, fixture: &Path, artifacts_yaml: &str) -> PathBuf {
    let artifact_path = create_tool_artifact(
        fixture,
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );
    let manifest_path = create_project(project, "capsule-allowlisted.wasm", artifacts_yaml, None);
    common::install_artifact_to_project(project, &artifact_path).success();
    manifest_path
}

#[test]
fn the_lock_mur_run_creates_marks_every_entry_operator() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = echo_tool_project(
        project.path(),
        fixture.path(),
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
    );
    let lock_path = project.path().join("murmur.lock");
    let _ = fs::remove_file(&lock_path);

    common::run_capsule(&home, &manifest_path).success();

    let lock = read_lockfile(&lock_path).unwrap();
    assert!(!lock.artifacts.is_empty());
    assert!(lock
        .artifacts
        .iter()
        .all(|entry| entry.origin == LockOrigin::Operator));
    let raw = fs::read_to_string(&lock_path).unwrap();
    assert_eq!(
        raw.matches("origin: operator").count(),
        lock.artifacts.len(),
        "{raw}"
    );
    assert!(!raw.contains("session:"), "{raw}");
}

/// Adds an entry a capsule pulled under `session` that murmur.yaml does not declare, and whose
/// artifact is in no store, so staging it would fail the run.
fn add_undeclared_runtime_pin(lock_path: &Path, session: &str) {
    let mut lock = read_lockfile(lock_path).unwrap();
    lock.artifacts.push(LockedArtifact {
        name: "local/pulled-elsewhere".to_string(),
        resolved_version: "3.0.0".to_string(),
        sha256: LockedSha256::any("pulled-hash".to_string()),
        origin: LockOrigin::Runtime {
            session: session.to_string(),
        },
        owner_id: None,
    });
    write_lockfile_atomic(lock_path, &lock).unwrap();
}

#[test]
fn an_undeclared_runtime_pin_is_not_staged() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = echo_tool_project(
        project.path(),
        fixture.path(),
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
    );
    common::run_capsule(&home, &manifest_path).success();
    let lock_path = project.path().join("murmur.lock");
    add_undeclared_runtime_pin(&lock_path, "ses_earlier");
    let before = fs::read(&lock_path).unwrap();

    common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    assert_eq!(fs::read(&lock_path).unwrap(), before);
}

/// A declared tool whose pin a capsule pulled cannot carry `gateway:`; the refusal comes before
/// the work folder exists.
#[test]
fn a_runtime_pin_declared_with_a_gateway_fails_with_e_run_043() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = echo_tool_project(
        project.path(),
        fixture.path(),
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
    );
    common::run_capsule(&home, &manifest_path).success();
    let lock_path = project.path().join("murmur.lock");
    let mut lock = read_lockfile(&lock_path).unwrap();
    lock.artifacts[0].origin = LockOrigin::Runtime {
        session: "ses_puller".to_string(),
    };
    write_lockfile_atomic(&lock_path, &lock).unwrap();
    fs::remove_dir_all(project.path().join("workdir")).unwrap();
    create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!(
            "  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n    gateway:\n      endpoint: https://api.example.com\n      api_key: test-key\n"
        ),
        None,
    );

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("error[E-RUN-043]"))
        .stderr(predicate::str::contains("ses_puller"))
        .stderr(predicate::str::contains(format!(
            "mur install {TOOL_ARTIFACT}@{TOOL_VERSION}"
        )));
    assert!(!project.path().join("workdir").exists());
}

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

fn create_project(
    project_dir: &Path,
    capsule_fixture: &str,
    artifacts_yaml: &str,
    capabilities_yaml: Option<&str>,
) -> PathBuf {
    let capabilities_block = capabilities_yaml
        .map(|yaml| format!("\ncapabilities:\n{yaml}"))
        .unwrap_or_default();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!("name: capsule\nversion: 0.0.1\nartifacts:\n{artifacts_yaml}{capabilities_block}"),
    )
    .unwrap();

    fs::copy(
        fixture_component(capsule_fixture),
        project_dir.join("capsule.wasm"),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

fn create_tool_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    tool_component_path: &Path,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(tool_component_path).unwrap())
        .unwrap();

    zip.finish().unwrap();
    artifact_path
}

fn write_registry_source_config(home: &Path, repo: &str) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        format!("registry:\n  sources:\n    - repo: {repo}\n"),
    )
    .unwrap();
}

// ── Door authentication ───────────────────────────────────────────────────────

/// `mur run` prints no token: the line after the url says the door is authenticated and names the
/// `mur token` command, and the token that command prints drives the door.
#[test]
fn an_authenticated_door_names_mur_token_after_the_url_line() {
    use common::door_capsule::{
        agent_project, driver_home, end_turn, message, rpc, wait_completed, MurRun,
        AUTHENTICATION_YAML, QUEUE_SLEEP_YAML,
    };
    let server = common::ScriptedServer::start(vec![end_turn(1, "done")]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        "door-run",
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let run = MurRun::start(
        home.path(),
        &project.path().join("murmur.yaml"),
        false,
        &[],
        &[],
    );

    let url_at = run
        .stdout_lines
        .iter()
        .position(|line| line.starts_with("murmur: url "))
        .expect("a url line");
    let session_id = run.session_id();
    assert_eq!(
        run.stdout_lines[url_at + 1],
        format!("murmur: auth bearer (mur token {session_id})")
    );
    assert_eq!(
        run.stdout_lines[url_at + 2],
        format!("session: {session_id}")
    );
    for line in &run.stdout_lines {
        assert!(!line.contains("mdt1."), "a token reached stdout: {line}");
    }

    let addr = run.url();
    let operator = run.token("operator");
    assert!(operator.starts_with("mdt1."), "{operator}");
    let sent = rpc(
        &addr,
        Some(&operator),
        "SendMessage",
        message("m-1", "hello"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_completed(&addr, Some(&operator), &task_id);

    let stderr = run.stderr();
    assert!(
        !stderr.contains("mdt1."),
        "a token reached stderr:\n{stderr}"
    );
}

/// A public door prints no `murmur: auth` line.
#[test]
fn a_public_door_prints_no_auth_line() {
    use common::door_capsule::{agent_project, driver_home, MurRun, QUEUE_SLEEP_YAML};
    let server = common::ScriptedServer::start(vec![]);
    let home = driver_home();
    let project = agent_project(&server.endpoint, "door-public", "", QUEUE_SLEEP_YAML);
    let run = MurRun::start(
        home.path(),
        &project.path().join("murmur.yaml"),
        false,
        &[],
        &[],
    );
    assert!(
        run.stdout_lines
            .iter()
            .any(|line| line.starts_with("murmur: url ")),
        "{:?}",
        run.stdout_lines
    );
    assert!(
        !run.stdout_lines
            .iter()
            .any(|line| line.starts_with("murmur: auth")),
        "{:?}",
        run.stdout_lines
    );
}

/// `--readiness-tokens` belongs to the `--json` readiness line, and is refused without it.
#[test]
fn readiness_tokens_without_json_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["run", "--readiness-tokens"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--json"), "{stderr}");
}

/// A delegating parent cannot declare `network.authentication`: nothing issues its children a
/// token for the door they post their outcomes to.
#[test]
fn authenticated_parent_refused() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        "name: auth-parent\nversion: 0.1.0\nartifacts: []\n\
         capabilities:\n  spawn:\n    allow: [local/worker]\n\
         network:\n  authentication:\n    scheme: bearer\n",
    )
    .unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["run", "--manifest", manifest.to_str().unwrap()])
        .current_dir(project.path())
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("E-MAN-003"), "{stderr}");
    assert!(stderr.contains("network.authentication"), "{stderr}");
    assert!(stderr.contains("capabilities.spawn.allow"), "{stderr}");
}

/// A `--workdir` beside an `outside/` directory holding one sentinel file, and a script capsule
/// project that completes on its own.
struct LinkedWorkdir {
    home: tempfile::TempDir,
    _fixture: tempfile::TempDir,
    _project: tempfile::TempDir,
    root: tempfile::TempDir,
    manifest_path: PathBuf,
}

impl LinkedWorkdir {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let artifact_path = create_tool_artifact(
            fixture.path(),
            TOOL_NAME,
            TOOL_VERSION,
            &fixture_component("echo-tool.wasm"),
        );
        let manifest_path = create_project(
            project.path(),
            "capsule-allowlisted.wasm",
            &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
            None,
        );
        common::install_artifact_to_project(project.path(), &artifact_path).success();
        fs::create_dir_all(root.path().join("work")).unwrap();
        fs::create_dir_all(root.path().join("outside")).unwrap();
        fs::write(root.path().join("outside/sentinel.txt"), "operator data").unwrap();
        Self {
            home,
            _fixture: fixture,
            _project: project,
            root,
            manifest_path,
        }
    }

    fn workdir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn outside(&self) -> PathBuf {
        self.root.path().join("outside")
    }

    fn link(&self, target: &Path, rel: &str) {
        std::os::unix::fs::symlink(target, self.workdir().join(rel)).unwrap();
    }

    fn run(&self, task: &str) -> assert_cmd::assert::Assert {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .args([
                "run",
                "--manifest",
                self.manifest_path.to_str().unwrap(),
                "--workdir",
                self.workdir().to_str().unwrap(),
                "--task",
                task,
                "--verbose",
            ])
            .assert()
    }

    /// The sentinel kept its bytes and `outside/` holds nothing else.
    fn assert_outside_unchanged(&self) {
        assert_eq!(
            fs::read_to_string(self.outside().join("sentinel.txt")).unwrap(),
            "operator data"
        );
        let names: Vec<_> = fs::read_dir(self.outside())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("sentinel.txt")]);
    }
}

/// A `task.md` in the workdir that is a link: `--task` goes to the runtime directory, so the link
/// is never written through, never read, and the file it names keeps its bytes.
#[test]
fn a_task_md_link_in_the_workdir_is_never_written_through() {
    let linked = LinkedWorkdir::new();
    linked.link(&linked.outside().join("sentinel.txt"), "task.md");

    linked
        .run("x")
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    linked.assert_outside_unchanged();
}

/// A `.murmur` that is a link to an outside directory: the runtime keeps nothing in the work
/// folder, so the launch runs to completion and nothing is written through the link.
#[test]
fn a_murmur_link_in_the_workdir_is_never_written_through() {
    let linked = LinkedWorkdir::new();
    linked.link(&linked.outside(), ".murmur");

    linked
        .run("x")
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    assert!(fs::symlink_metadata(linked.workdir().join(".murmur"))
        .unwrap()
        .file_type()
        .is_symlink());
    linked.assert_outside_unchanged();
}

/// A `murmur.yaml` that is a dangling link into `outside/`: the manifest is copied into the
/// runtime directory, never into the workdir, so the file the link names is never created.
#[test]
fn a_dangling_manifest_link_in_the_workdir_is_never_written_through() {
    let linked = LinkedWorkdir::new();
    linked.link(&linked.outside().join("not-yet.yaml"), "murmur.yaml");

    linked.run("x").success();

    linked.assert_outside_unchanged();
}

/// With no link anywhere, the same `--workdir` launch runs to completion and writes neither its
/// task nor the manifest into the workdir.
#[test]
fn workdir_path_with_no_links_runs_to_completion() {
    let linked = LinkedWorkdir::new();

    linked
        .run("the task")
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    assert!(!linked.workdir().join("task.md").exists());
    assert!(!linked.workdir().join("murmur.yaml").exists());
    assert!(!linked.workdir().join(".murmur").exists());
    linked.assert_outside_unchanged();
}

// ── A script capsule's record ─────────────────────────────────────────────────

/// The startup line `marker` (`session: `, `records: `) of a successful `mur run --verbose`.
fn startup_value(stdout: &str, marker: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .unwrap_or_else(|| panic!("no `{marker}` line in:\n{stdout}"))
        .trim()
        .to_string()
}

/// The `capsule-allowlisted` project, `echo-tool` installed, with `extra_yaml` appended to its
/// manifest.
fn allowlisted_echo_project(fixture: &Path, project: &Path, extra_yaml: &str) -> PathBuf {
    let artifact_path = create_tool_artifact(
        fixture,
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );
    let manifest_path = create_project(
        project,
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_ARTIFACT}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    if !extra_yaml.is_empty() {
        let mut manifest = fs::read_to_string(&manifest_path).unwrap();
        manifest.push_str(extra_yaml);
        fs::write(&manifest_path, manifest).unwrap();
    }
    common::install_artifact_to_project(project, &artifact_path).success();
    manifest_path
}

/// A script capsule's trace is a session like an agent's: `session_start` first, one `tool_call`
/// per `invoke` hanging off it, and `session_end` last, saying the run returned.
#[test]
fn a_script_capsule_writes_a_session_frame_and_a_tool_call_per_invoke() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = allowlisted_echo_project(fixture.path(), project.path(), "");

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    let session_id = startup_value(&stdout, "session: ");
    let records = PathBuf::from(startup_value(&stdout, "records: "));
    let lines = common::read_whole_trace(&records.join("trace.jsonl"));

    let start = &lines[0];
    assert_eq!(start["event_type"], "session_start", "{start}");
    assert_eq!(start["capsule_name"], "capsule");
    assert_eq!(start["capsule_version"], "0.0.1");
    assert_eq!(start["session_id"], session_id.as_str());
    assert_eq!(start["model"], "");
    assert!(start["max_turns"].is_null(), "{start}");
    assert!(start["model_limits"].is_null(), "{start}");
    assert_eq!(start["tools_declared"], serde_json::json!(["echo-tool"]));
    for key in [
        "containment_declared",
        "containment_achieved",
        "userns_grant",
        "workdir_exec",
    ] {
        assert!(start.get(key).is_some(), "no {key} in {start}");
    }
    assert!(start["effective_grants"].is_object(), "{start}");

    let calls: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["event_type"] == "tool_call")
        .collect();
    assert_eq!(calls.len(), 1, "{lines:#?}");
    let call = calls[0];
    assert_eq!(call["tool_name"], "echo-tool");
    assert_eq!(call["status"], "ok");
    assert_eq!(call["turn"], 0);
    assert!(call["task_id"].is_null(), "{call}");
    assert!(call["tool_call_id"].is_null(), "{call}");
    assert_eq!(call["input"], "hello");
    assert_eq!(call["input_bytes"], 5);
    assert!(call.get("error").is_none(), "{call}");
    assert_eq!(call["parent_id"], start["event_id"]);

    let end = lines.last().unwrap();
    assert_eq!(end["event_type"], "session_end", "{end}");
    assert_eq!(end["exit_status"], "ok");
    assert_eq!(end["total_tool_calls"], 1);
    assert_eq!(end["total_turns"], 0);
    let parentless: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["parent_id"].is_null())
        .collect();
    assert_eq!(parentless, vec![start], "only session_start is a root");

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "trace",
            "show",
            records.join("trace.jsonl").to_str().unwrap(),
        ])
        .assert()
        .success();
}

/// A guest that traps ends its session `trapped`, after the refused `invoke` that made it trap is
/// recorded, and leaves no running record behind.
#[test]
fn a_trapped_script_capsule_ends_its_session_trapped() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = create_project(project.path(), "capsule-allowlisted.wasm", " []\n", None);

    common::run_capsule(&home, &manifest_path)
        .code(1)
        .stdout(predicate::str::contains("status:  trapped"));

    let trace = common::find_file(&common::records_root(home.path()), "trace.jsonl")
        .expect("the session wrote a trace");
    let lines = common::read_whole_trace(&trace);
    assert_eq!(lines[0]["event_type"], "session_start");
    let calls: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["event_type"] == "tool_call")
        .collect();
    assert_eq!(calls.len(), 1, "{lines:#?}");
    assert_eq!(calls[0]["status"], "error");
    assert!(
        calls[0]["error"]
            .as_str()
            .unwrap()
            .contains("not declared in manifest allowlist"),
        "{}",
        calls[0]
    );
    let end = lines.last().unwrap();
    assert_eq!(end["event_type"], "session_end", "{end}");
    assert_eq!(end["exit_status"], "trapped");
    assert_eq!(end["total_tool_calls"], 1);

    let session_id = lines[0]["session_id"].as_str().unwrap();
    assert!(!home
        .path()
        .join(".murmur")
        .join("running")
        .join(format!("{session_id}.json"))
        .exists());
}

/// Every OTLP request body posted to the returned port, as it arrives; each is answered `200`.
fn otlp_collector() -> (
    u16,
    std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    use std::io::{BufRead, BufReader, Read};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            if request_line.starts_with("POST /v1/traces ") {
                kept.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (port, bodies)
}

/// The string attribute `key` of an OTLP span.
fn span_attribute<'a>(span: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)?["value"]["stringValue"]
        .as_str()
}

/// With `observability.otel_endpoint` set, a script session posts its `capsule.session` span and
/// one `capsule.tool_call` child per `invoke`, in one trace.
#[test]
fn a_script_capsule_posts_a_session_span_and_a_tool_call_span() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let (port, bodies) = otlp_collector();
    let manifest_path = allowlisted_echo_project(
        fixture.path(),
        project.path(),
        &format!("observability:\n  otel_endpoint: http://127.0.0.1:{port}\n"),
    );

    common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    let spans: Vec<serde_json::Value> = bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| {
            body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let named = |name: &str| -> Vec<&serde_json::Value> {
        spans.iter().filter(|span| span["name"] == name).collect()
    };
    let sessions = named("capsule.session");
    let calls = named("capsule.tool_call");
    assert_eq!(sessions.len(), 1, "{spans:#?}");
    assert_eq!(calls.len(), 1, "{spans:#?}");
    let (session, call) = (sessions[0], calls[0]);
    assert_eq!(span_attribute(session, "exit_status"), Some("ok"));
    assert_eq!(span_attribute(call, "tool_name"), Some("echo-tool"));
    assert_eq!(span_attribute(call, "status"), Some("ok"));
    assert_eq!(call["parentSpanId"], session["spanId"]);
    assert_eq!(call["traceId"], session["traceId"]);
}

/// A project declaring `capabilities.spawn` with the given body, in a fresh directory.
fn spawn_project(spawn_yaml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!("name: demo\nversion: 0.1.0\ncapabilities:\n  spawn:\n{spawn_yaml}"),
    )
    .unwrap();
    fs::write(dir.path().join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
    dir
}

fn explain_scope_of(project: &Path, extra: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project)
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
        .args(extra)
        .output()
        .unwrap()
}

const SNAPSHOT_JSON: &str = r#"{"source":"/home/you/project","files":12,"dirs":3,"symlinks":1,"repointed_symlinks":0,"bytes":40960,"skipped_count":1,"skipped":[{"path":"run/pipe","reason":"fifo"}]}"#;

/// The parent's report lists `capabilities.spawn.workdir_snapshot` beside `spawn allow`, and a
/// session started in no copy reports `parent_workdir: null`.
#[test]
fn explain_scope_reports_spawn_workdir_snapshot() {
    let project =
        spawn_project("    allow: [local/reviewer]\n    workdir_snapshot: [local/reviewer]\n");

    let json = explain_scope_of(project.path(), &["--json"]);
    assert!(json.status.success(), "{json:?}");
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(
        report["spawn_workdir_snapshot"],
        serde_json::json!(["local/reviewer"])
    );
    assert!(report.as_object().unwrap().contains_key("parent_workdir"));
    assert_eq!(report["parent_workdir"], serde_json::Value::Null);

    let human = String::from_utf8(explain_scope_of(project.path(), &[]).stdout).unwrap();
    assert!(
        human.contains(
            "  spawn allow:\n    - local/reviewer\n  spawn workdir snapshot:\n    - local/reviewer\n"
        ),
        "{human}"
    );

    let undeclared = spawn_project("    allow: [local/reviewer]\n");
    let report: serde_json::Value =
        serde_json::from_slice(&explain_scope_of(undeclared.path(), &["--json"]).stdout).unwrap();
    assert_eq!(report["spawn_workdir_snapshot"], serde_json::json!([]));
}

/// A child told what its parent copied reports it under `parent_workdir`, with this host's
/// restriction; the human report prints a `Parent work folder` block.
#[test]
fn explain_scope_reports_parent_workdir_from_the_hidden_flag() {
    let project = spawn_project("    allow: []\n");
    let workdir = tempfile::tempdir().unwrap();
    let workdir_arg = workdir.path().to_str().unwrap();
    let flags = [
        "--workdir",
        workdir_arg,
        "--parent-workdir-snapshot",
        SNAPSHOT_JSON,
    ];

    let mut json_flags = flags.to_vec();
    json_flags.push("--json");
    let output = explain_scope_of(project.path(), &json_flags);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let parent = &report["parent_workdir"];
    assert_eq!(parent["source"], "/home/you/project");
    assert_eq!(parent["files"], 12);
    assert_eq!(parent["bytes"], 40960);
    assert_eq!(parent["skipped"][0]["reason"], "fifo");
    assert_eq!(
        parent["restriction"], report["filesystem_boundary"]["restriction"],
        "{report}"
    );

    let human = String::from_utf8(explain_scope_of(project.path(), &flags).stdout).unwrap();
    assert!(
        human.contains(
            "\nParent work folder\n  source:      /home/you/project\n  copied:      12 files, 3 \
             directories, 1 symlinks (0 re-pointed), 40960 bytes\n  skipped:     1\n    - \
             run/pipe (fifo)\n  restriction: "
        ),
        "{human}"
    );
}

/// The flag takes only the snapshot JSON a parent's runtime writes, needs `--workdir`, conflicts
/// with `--roster` and is hidden from `--help`.
#[test]
fn parent_workdir_snapshot_flag_is_hidden_and_validated() {
    let project = spawn_project("    allow: []\n");
    let workdir = tempfile::tempdir().unwrap();
    let workdir_arg = workdir.path().to_str().unwrap();

    let malformed = explain_scope_of(
        project.path(),
        &[
            "--workdir",
            workdir_arg,
            "--parent-workdir-snapshot",
            "{\"files\":\"many\"}",
        ],
    );
    assert!(!malformed.status.success());
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(stderr.contains("--parent-workdir-snapshot"), "{stderr}");

    let no_workdir = explain_scope_of(
        project.path(),
        &["--parent-workdir-snapshot", SNAPSHOT_JSON],
    );
    assert!(!no_workdir.status.success());
    assert!(
        String::from_utf8_lossy(&no_workdir.stderr).contains("--workdir"),
        "{no_workdir:?}"
    );

    let roster = Command::cargo_bin("mur")
        .unwrap()
        .args([
            "run",
            "--roster",
            "roster.yaml",
            "--parent-workdir-snapshot",
            SNAPSHOT_JSON,
        ])
        .output()
        .unwrap();
    assert!(!roster.status.success());
    assert!(
        String::from_utf8_lossy(&roster.stderr).contains("cannot be used with"),
        "{roster:?}"
    );

    let help = Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&help.stdout).contains("parent-workdir-snapshot"));
}
