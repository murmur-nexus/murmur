//! `E-CAP-016`: a `capabilities.env.allow` entry the credential backstop strips from every guest
//! environment refuses the capsule, from `mur run --explain-scope`, from a real `mur run` before
//! any registry work, and as a warning from `mur doctor`.
//!
//! What is measured is which names are refused, with which pattern and source, in what order, and
//! that the verdict never depends on — or prints — the host's value. The real `mur run` case uses
//! an empty artifact store: a missing-artifact error there would mean the refusal came after
//! registry resolution.

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

const CODE: &str = "E-CAP-016";

fn project(dir: &Path, manifest: &str) {
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn env_allow_manifest(names: &[&str], extra: &str) -> String {
    let entries: String = names
        .iter()
        .map(|name| format!("      - {name}\n"))
        .collect();
    format!("name: demo\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n{entries}{extra}")
}

/// `mur run --explain-scope`, asserted to fail, returning its stderr.
fn refused_explain_scope_stderr(
    home: &TempDir,
    project_dir: &Path,
    env: &[(&str, &str)],
) -> String {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .current_dir(project_dir)
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"]);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.assert().failure().get_output().stderr.clone();
    String::from_utf8(output).unwrap()
}

fn code_lines(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|line| line.contains(CODE)).collect()
}

/// A built-in pattern refuses the capsule by name and pattern, whether or not the host holds the
/// variable, and the refused manifest prints no grant warning.
#[test]
fn a_builtin_backstop_name_refuses_explain_scope_whatever_the_host_holds() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), &env_allow_manifest(&["GITHUB_TOKEN"], ""));

    let unset = refused_explain_scope_stderr(&home, dir.path(), &[]);
    let set = refused_explain_scope_stderr(&home, dir.path(), &[("GITHUB_TOKEN", "x")]);

    for stderr in [&unset, &set] {
        let lines = code_lines(stderr);
        assert_eq!(lines.len(), 1, "stderr was: {stderr}");
        assert!(
            lines[0].contains("'GITHUB_TOKEN' (credential backstop pattern 'GITHUB_TOKEN')"),
            "{}",
            lines[0]
        );
        assert!(!stderr.contains("W-SEC-024"), "stderr was: {stderr}");
    }
    assert_eq!(code_lines(&unset), code_lines(&set));
    assert!(
        !set.contains("'x'") && !set.contains("=x"),
        "stderr was: {set}"
    );
}

/// Names that are not credential-shaped at all are refused when a prefix pattern covers them, in
/// one error, in declaration order.
#[test]
fn every_stripped_name_is_named_in_one_error_in_declaration_order() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        &env_allow_manifest(&["AWS_REGION", "DOCKER_HOST"], ""),
    );

    let stderr = refused_explain_scope_stderr(&home, dir.path(), &[]);
    let lines = code_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    let aws = lines[0]
        .find("'AWS_REGION' (credential backstop pattern 'AWS_*')")
        .unwrap_or_else(|| panic!("{}", lines[0]));
    let docker = lines[0]
        .find("'DOCKER_HOST' (credential backstop pattern 'DOCKER_*')")
        .unwrap_or_else(|| panic!("{}", lines[0]));
    assert!(aws < docker, "{}", lines[0]);
}

/// A manifest's own `capabilities.shell.strip_env` pattern is attributed to that key.
#[test]
fn a_manifest_strip_env_pattern_is_attributed_to_capabilities_shell_strip_env() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        // `capabilities.shell` is rejected without a non-empty `allow`, so the block that carries
        // `strip_env` names a binary too.
        &env_allow_manifest(
            &["MY_SERVICE_SECRET"],
            "  shell:\n    allow:\n      - echo\n    strip_env:\n      - \"*_SERVICE_SECRET\"\n",
        ),
    );

    let stderr = refused_explain_scope_stderr(&home, dir.path(), &[]);
    let lines = code_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    assert!(
        lines[0].contains(
            "'MY_SERVICE_SECRET' (capabilities.shell.strip_env pattern '*_SERVICE_SECRET')"
        ),
        "{}",
        lines[0]
    );
}

/// A repeated entry is named once, and a kept name beside it is never named.
#[test]
fn a_repeated_entry_is_named_once_and_a_kept_name_never() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        &env_allow_manifest(&["NPM_TOKEN", "TZ", "NPM_TOKEN", "KUBECONFIG"], ""),
    );

    let stderr = refused_explain_scope_stderr(&home, dir.path(), &[]);
    let lines = code_lines(&stderr);

    assert_eq!(lines.len(), 1, "stderr was: {stderr}");
    // Each entry is rendered `'NAME' (… pattern '…')`, so an exact-name pattern repeats the name;
    // counting the entry prefix counts entries.
    assert_eq!(lines[0].matches("'NPM_TOKEN' (").count(), 1, "{}", lines[0]);
    let npm = lines[0].find("'NPM_TOKEN' (").unwrap();
    let kube = lines[0]
        .find("'KUBECONFIG' (")
        .unwrap_or_else(|| panic!("{}", lines[0]));
    assert!(npm < kube, "{}", lines[0]);
    assert!(!lines[0].contains("'TZ'"), "{}", lines[0]);
}

/// A real launch with an uninstalled tool artifact and an empty store is refused by `E-CAP-016`
/// rather than by the missing artifact, and leaves no session directory behind.
#[test]
fn a_real_run_refuses_before_any_registry_work() {
    let home = TempDir::new().unwrap();
    let project_dir = TempDir::new().unwrap();
    let manifest = project_dir.path().join("murmur.yaml");
    fs::write(
        &manifest,
        "name: stripped-env-capsule\nversion: 0.1.0\ncapabilities:\n  env:\n    allow:\n      \
         - GITHUB_TOKEN\nartifacts:\n  - name: uninstalled-tool\n    version: 0.1.0\n    \
         runtime: tool\ninference:\n  transport: http\n  endpoint: http://127.0.0.1:1\n  \
         model: test-model\n  api_key: test-key\n  driver:\n    \
         artifact: murmur-driver-anthropic\n",
    )
    .unwrap();

    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env("GITHUB_TOKEN", "a-real-looking-token")
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            "anything",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();

    assert!(stderr.contains(CODE), "stderr was: {stderr}");
    assert!(stderr.contains("'GITHUB_TOKEN'"), "stderr was: {stderr}");
    assert!(
        !stderr.contains("missing artifacts") && !stderr.contains("E-REG-001"),
        "the refusal must precede registry resolution: {stderr}"
    );
    assert!(
        !stderr.contains("a-real-looking-token"),
        "stderr was: {stderr}"
    );
    assert!(
        !project_dir.path().join("workdir").exists(),
        "a refused launch creates no session directory: {stderr}"
    );
}

/// `mur doctor` reports what `mur run` will refuse as a warning and carries on with its checklist.
#[test]
fn doctor_warns_that_run_will_refuse_and_keeps_going() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), &env_allow_manifest(&["GITHUB_TOKEN"], ""));

    // Doctor's exit status depends on the artifact checklist, so the stream is taken without
    // asserting on it.
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .current_dir(dir.path())
        .arg("doctor")
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    let warning = stderr
        .lines()
        .position(|line| line.starts_with("[mur doctor] warning[E-CAP-016]:"))
        .unwrap_or_else(|| panic!("stderr was: {stderr}"));
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(
        lines[warning].contains("'GITHUB_TOKEN'"),
        "{}",
        lines[warning]
    );
    assert!(
        lines[warning + 1].contains("`mur run` will refuse"),
        "stderr was: {stderr}"
    );
    assert!(!stderr.contains("W-SEC-024"), "stderr was: {stderr}");
    // The preopen report comes after the manifest-only prologue the warning is part of.
    assert!(
        stdout.contains("Filesystem preopens") || stderr.contains("Filesystem preopens"),
        "doctor stopped after the warning; stdout: {stdout}\nstderr: {stderr}"
    );
}
