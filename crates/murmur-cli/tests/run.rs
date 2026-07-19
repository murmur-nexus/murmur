#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use murmur_artifact::read_lockfile;
use predicates::prelude::*;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "echo-tool";
const TOOL_VERSION: &str = "0.1.0";

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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
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
    let first_workdir = parse_workdir_from_stdout(&first_stdout);
    assert_eq!(
        fs::read_to_string(first_workdir.join("out/result.txt")).unwrap(),
        "ok"
    );

    let lock_path = project.path().join("murmur.lock");
    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock
        .artifact_for(TOOL_NAME)
        .expect("lock entry for echo-tool");
    assert_eq!(entry.resolved_version, TOOL_VERSION);
    assert!(!entry.sha256.wasm.is_empty());

    // Change the manifest version to a non-existent one. The second run should still
    // succeed by honoring the existing lock pin.
    fs::write(
        &manifest_path,
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: {TOOL_NAME}\n    version: 9.9.9\n"
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
    let second_workdir = parse_workdir_from_stdout(&second_stdout);
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let stdout = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(stdout).unwrap());
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
        "  - name: missing-tool\n    version: 0.0.1\n",
        None,
    );

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains(
            "missing artifacts: missing-tool@0.0.1",
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        None,
    );

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains(format!(
            "missing artifacts: {TOOL_NAME}@{TOOL_VERSION}"
        )))
        .stderr(predicate::str::contains("mur install"));
}

/// `mur run` does not auto-pull. Commit 3f5c85b ("fix: remove offline flag") deleted the
/// `--offline` flag and, with it, `ensure_artifacts_available_locally` — the path that used to
/// consult the configured registry source chain and fetch missing artifacts mid-run. Installing
/// is now exclusively `mur install`'s job; `mur run` only ever checks.
///
/// This test pins that removal: even with a source chain configured, a missing artifact fails
/// fast with E-RUN-008 and no network request is made. It replaces the former
/// `run_auto_pulls_missing_artifact_when_source_chain_configured`, which still asserted the
/// removed behavior and hung — its mock server blocked in `accept()` forever waiting on a
/// request `mur run` no longer makes.
///
/// `MUR_GITHUB_API_BASE` points at a bound-but-never-served listener: nothing accepts, so any
/// regression that reintroduces a pull hangs on connect rather than silently passing. The
/// GitHub source's real request/response behavior is covered by the unit tests in
/// `crates/murmur-cli/src/source/mod.rs`, which have their own mock server.
#[test]
fn run_does_not_auto_pull_missing_artifact_even_when_source_chain_configured() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    write_registry_source_config(home.path(), "acme/artifacts");

    // Bound but never accepted: proves `mur run` issues no request against the source chain.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());

    let manifest_path = create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
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
            "missing artifacts: {TOOL_NAME}@{TOOL_VERSION}"
        )));

    let installed_zip = project.path().join(format!(
        ".murmur/artifacts/{TOOL_NAME}/{TOOL_VERSION}/{TOOL_NAME}-{TOOL_VERSION}.mur.zip"
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    common::run_capsule(&home, &manifest_path).success();

    let installed_zip = project.path().join(format!(
        ".murmur/artifacts/{TOOL_NAME}/{TOOL_VERSION}/{TOOL_NAME}-{TOOL_VERSION}.mur.zip"
    ));
    fs::write(installed_zip, b"tampered").unwrap();

    common::run_capsule(&home, &manifest_path)
        .failure()
        .stderr(predicate::str::contains(format!(
            "artifact integrity check failed for {TOOL_NAME}@{TOOL_VERSION}"
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        Some("  network:\n    allow:\n      - https://allowed.example.com\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "denied"
    );
}

#[test]
fn run_allowlisted_network_host_is_not_denied_at_handle_time() {
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        Some("  network:\n    allow:\n      - https://blocked.example.com\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        capabilities_yaml,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule_with_env(&home, &manifest_path, extra_env)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
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

#[test]
fn run_with_env_allowlisted_credential_var_is_still_stripped() {
    let result = run_env_echo(
        Some("  env:\n    allow:\n      - GITHUB_TOKEN\n"),
        &[("GITHUB_TOKEN", "leaked-token")],
    );

    assert!(
        result.contains("GITHUB_TOKEN=absent"),
        "credential backstop did not override the allowlist: {result}"
    );
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
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
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        Some("  filesystem:\n    scope: .\n"),
    );
    common::install_artifact_to_project(project.path(), &artifact_path).success();

    let output = common::run_capsule(&home, &manifest_path)
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let workdir = parse_workdir_from_stdout(&String::from_utf8(output).unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join("out/result.txt")).unwrap(),
        "blocked"
    );
    assert!(!project.path().join("outside.txt").exists());
}

/// Sets up a manifest-only "agent capsule" project (murmur.yaml, no capsule.wasm
/// needed since `inference` is present) whose `inference.api_key` references an env
/// var, plus a workspace-root `.env` that sets that var. The `murmur.yaml` doubles as
/// the workspace-root marker that `.env` auto-loading keys on. The manifest also
/// declares one uninstalled tool artifact so that, once manifest parsing succeeds,
/// `mur run` fails fast with E-RUN-008 rather than starting an HTTP server and
/// blocking forever.
fn create_dotenv_project(project_dir: &Path, env_var: &str) -> PathBuf {
    fs::write(
        project_dir.join(".env"),
        format!("{env_var}=from-dotenv\n"),
    )
    .unwrap();

    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: missing-tool\n    version: 0.0.1\ninference:\n  transport: http\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  api_key: ${{{env_var}}}\n  driver:\n    artifact: dummy-driver\n"
        ),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

#[test]
fn run_no_env_file_skips_dotenv_and_fails_manifest_resolution() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_dotenv_project(project.path(), "CI_TEST_VAR");

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
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
        .stderr(predicate::str::contains("inference.api_key"))
        .stderr(predicate::str::contains("${CI_TEST_VAR}"));
}

#[test]
fn run_without_no_env_file_flag_loads_dotenv_and_resolves_manifest() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_dotenv_project(project.path(), "CI_TEST_VAR");

    // No --no-env-file: .env is auto-loaded, so CI_TEST_VAR resolves and manifest
    // parsing succeeds. The run then fails later, at the uninstalled-artifact check
    // (E-RUN-008) rather than at manifest resolution (E-MAN-003) — proof that
    // inference.api_key was resolved via the auto-loaded .env file.
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("CI_TEST_VAR")
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-RUN-008"))
        .stderr(predicate::str::contains("missing-tool@0.0.1"));
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
        format!(
            "registry:\n  default: official\n  sources:\n    - name: official\n      type: github\n      repo: {repo}\n"
        ),
    )
    .unwrap();
}

fn parse_workdir_from_stdout(stdout: &str) -> PathBuf {
    let marker = "workdir: ";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("missing '{marker}' in stdout: {stdout}"));

    let after = &stdout[start + marker.len()..];
    let workdir = after.lines().next().unwrap_or_default().trim();
    PathBuf::from(workdir)
}
