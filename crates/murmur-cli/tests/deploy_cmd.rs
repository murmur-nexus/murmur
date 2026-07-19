// Integration tests for mur deploy/destroy/ps.
// Tests cover argument validation and state file I/O.
// No real VMs are provisioned.
#![cfg(feature = "beta-mur-deploy")]

use std::fs;

use assert_cmd::Command;
use murmur_artifact::RuntimeManifest;
use serde_json::Value;
use tempfile::tempdir;

// ─── helpers ──────────────────────────────────────────────────────────────────

fn make_manifest(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("murmur.yaml");
    fs::write(&p, "name: test-agent\nversion: 0.1.0\nruntime: agent\n").unwrap();
    p
}

fn deployments_json_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".murmur").join("deployments.json")
}

fn write_deployment(home: &std::path::Path, job_id: &str, ip: &str) {
    let json_path = deployments_json_path(home);
    fs::create_dir_all(json_path.parent().unwrap()).unwrap();
    let arr = serde_json::json!([{
        "job_id": job_id,
        "provider": "manual",
        "provider_vm_id": "",
        "provider_key_id": "",
        "region": "",
        "ip": ip,
        "url": format!("https://{}:8080", ip),
        "manifest_path": "/tmp/murmur.yaml",
        "started_at": "2026-06-03T00:00:00Z",
        "status": "running"
    }]);
    fs::write(&json_path, serde_json::to_string_pretty(&arr).unwrap()).unwrap();
}

/// Write ~/.murmur/config.yaml with `mur-deploy` enabled so the binary accepts
/// deploy/destroy/ps commands during integration tests.
fn enable_deploy_beta(home: &std::path::Path) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "beta:\n  enabled:\n    - mur-deploy\n",
    )
    .unwrap();
}

// ─── mur ps ───────────────────────────────────────────────────────────────────

#[test]
fn ps_empty_home_prints_no_deployments() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no deployments"), "got: {stdout}");
}

#[test]
fn ps_lists_deployment_from_json() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    write_deployment(dir.path(), "job-abc-123", "1.2.3.4");

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("job-abc-123"), "got: {stdout}");
    assert!(stdout.contains("1.2.3.4"), "got: {stdout}");
    assert!(stdout.contains("https://1.2.3.4:8080"), "got: {stdout}");
}

#[test]
fn ps_multiple_deployments_shows_all() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let json_path = deployments_json_path(dir.path());
    fs::create_dir_all(json_path.parent().unwrap()).unwrap();
    let arr = serde_json::json!([
        {
            "job_id": "job-111",
            "provider": "manual",
            "provider_vm_id": "",
            "provider_key_id": "",
            "region": "",
            "ip": "10.0.0.1",
            "url": "https://10.0.0.1:9000",
            "manifest_path": "/a",
            "started_at": "2026-06-03T00:00:00Z",
            "status": "running"
        },
        {
            "job_id": "job-222",
            "provider": "manual",
            "provider_vm_id": "",
            "provider_key_id": "",
            "region": "",
            "ip": "10.0.0.2",
            "url": "https://10.0.0.2:9001",
            "manifest_path": "/b",
            "started_at": "2026-06-03T01:00:00Z",
            "status": "running"
        }
    ]);
    fs::write(&json_path, serde_json::to_string_pretty(&arr).unwrap()).unwrap();

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("job-111"), "got: {stdout}");
    assert!(stdout.contains("job-222"), "got: {stdout}");
}

// ─── argument validation ──────────────────────────────────────────────────────

#[test]
fn missing_manifest_fails_before_connecting() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host", "1.2.3.4",
            "--manifest", "/nonexistent/path/murmur.yaml",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("E-IO-001") || stderr.contains("manifest not found"),
        "expected manifest-not-found error, got: {stderr}"
    );
}

#[test]
fn missing_workdir_fails_before_connecting() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let manifest = make_manifest(dir.path());

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host", "1.2.3.4",
            "--manifest", manifest.to_str().unwrap(),
            "--workdir", "/nonexistent/workdir-xyz",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("E-IO-001") || stderr.contains("workdir not found"),
        "expected workdir-not-found error, got: {stderr}"
    );
}

#[test]
fn missing_mur_binary_fails_before_connecting() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let manifest = make_manifest(dir.path());

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host", "1.2.3.4",
            "--manifest", manifest.to_str().unwrap(),
            "--mur-binary", "/nonexistent/mur-linux",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("E-IO-001") || stderr.contains("--mur-binary not found"),
        "expected mur-binary-not-found error, got: {stderr}"
    );
}

#[test]
fn invalid_env_var_format_fails_before_connecting() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let manifest = make_manifest(dir.path());

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host", "1.2.3.4",
            "--manifest", manifest.to_str().unwrap(),
            "--env", "MISSING_EQUALS",
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("KEY=VALUE") || stderr.contains("MISSING_EQUALS"),
        "expected KEY=VALUE format error, got: {stderr}"
    );
}

// ─── artifact staging ─────────────────────────────────────────────────────────

#[test]
fn missing_artifact_fails_before_ssh_attempt() {
    let dir = tempdir().unwrap();

    // Write config with no sources — chain is empty, staging fails immediately without network.
    // Also enable mur-deploy so the binary accepts the deploy subcommand.
    let config_dir = dir.path().join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "registry:\n  sources: []\nbeta:\n  enabled:\n    - mur-deploy\n",
    )
    .unwrap();

    // Manifest that declares an artifact which cannot be resolved (no sources, no local file)
    let manifest_p = dir.path().join("murmur.yaml");
    fs::write(
        &manifest_p,
        "name: my-agent\nversion: 0.1.0\nartifacts:\n  - name: my-missing-tool\n    version: 9.9.9\n",
    )
    .unwrap();

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host",
            "192.0.2.1", // TEST-NET; never reached because staging fails first
            "--manifest",
            manifest_p.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("my-missing-tool"),
        "error must name the missing artifact, got: {stderr}"
    );
}

// ─── mur_version field ───────────────────────────────────────────────────────

#[test]
fn mur_version_parses_from_manifest_yaml() {
    let manifest = RuntimeManifest::from_yaml_str(
        "name: cap\nversion: 0.1.0\nartifacts: []\nmur_version: \"0.4.5\"\n",
    )
    .unwrap();
    assert_eq!(manifest.mur_version, Some("0.4.5".to_string()));
}

#[test]
fn mur_version_absent_is_none() {
    let manifest =
        RuntimeManifest::from_yaml_str("name: cap\nversion: 0.1.0\nartifacts: []\n").unwrap();
    assert_eq!(manifest.mur_version, None);
}

// ─── manifest-referenced file upload ─────────────────────────────────────────

#[test]
fn missing_system_prompt_file_fails_before_ssh_attempt() {
    let dir = tempdir().unwrap();

    // Write config with no sources so artifact staging is a no-op (fast fail path).
    // Also enable mur-deploy so the binary accepts the deploy subcommand.
    let config_dir = dir.path().join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "registry:\n  sources: []\nbeta:\n  enabled:\n    - mur-deploy\n",
    )
    .unwrap();

    // Manifest that references instructions.md via system_prompt_file — do NOT create the file.
    let manifest_p = dir.path().join("murmur.yaml");
    fs::write(
        &manifest_p,
        "name: my-agent\nversion: 0.1.0\nartifacts: []\n\
         inference:\n  endpoint: http://localhost:8080\n  model: gpt-4\n  \
         system_prompt_file: instructions.md\n  \
         driver:\n    artifact: murmur-driver-openai\n",
    )
    .unwrap();

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args([
            "deploy",
            "--host",
            "192.0.2.1", // TEST-NET; never reached because file check fails first
            "--manifest",
            manifest_p.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("instructions.md"),
        "error must mention the missing file, got: {stderr}"
    );
}

// ─── deployments.json schema ──────────────────────────────────────────────────

#[test]
fn deployments_json_schema_has_required_fields() {
    let dir = tempdir().unwrap();
    write_deployment(dir.path(), "schema-test-job", "9.9.9.9");

    let json_path = deployments_json_path(dir.path());
    let raw = fs::read_to_string(&json_path).unwrap();
    let arr: Vec<Value> = serde_json::from_str(&raw).unwrap();
    let r = &arr[0];

    for field in &[
        "job_id", "provider", "provider_vm_id", "provider_key_id",
        "region", "ip", "url", "manifest_path", "started_at", "status",
    ] {
        assert!(r.get(field).is_some(), "deployments.json missing field: {field}");
    }
}
