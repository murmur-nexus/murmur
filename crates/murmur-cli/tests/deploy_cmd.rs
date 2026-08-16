// Integration tests for mur deploy/destroy/ps.
// Tests cover argument validation and state file I/O.
// No real VMs are provisioned.
#![cfg(feature = "beta-mur-deploy")]

use std::fs;

use assert_cmd::Command;
use murmur_artifact::RuntimeManifest;
use serde_json::Value;
use tempfile::tempdir;

// ─── fixture ids ──────────────────────────────────────────────────────────────

// Shaped like what `mur deploy` mints: `format!("dep_{}", Uuid::now_v7().simple())`, i.e. the
// `dep_` prefix followed by 32 lowercase hex characters. Tests slice prefixes out of these, so
// the length matters as much as the prefix.
const DEPLOYMENT_ID: &str = "dep_019e9d85f1a37b4e9c0d2f6a8b3c1d5e";
const OTHER_DEPLOYMENT_ID: &str = "dep_019e9d861c4a7f2b8e5d3a9c6b0f4e17";

// These two share the first 12 characters ("dep_019e9d87") so a short prefix is ambiguous.
const AMBIGUOUS_ID_A: &str = "dep_019e9d87a1b2c3d4e5f60718293a4b5c";
const AMBIGUOUS_ID_B: &str = "dep_019e9d87f0e1d2c3b4a5968776655443";

// ─── helpers ──────────────────────────────────────────────────────────────────

fn make_manifest(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("murmur.yaml");
    fs::write(&p, "name: test-agent\nversion: 0.1.0\nruntime: agent\n").unwrap();
    p
}

fn deployments_json_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".murmur").join("deployments.json")
}

/// Write `deployments.json` in the *current* on-disk format: the id field is `deployment_id`
/// and every id carries the `dep_` prefix `mur deploy` mints. Takes `(deployment_id, ip)` pairs.
fn write_deployments(home: &std::path::Path, deployments: &[(&str, &str)]) {
    let json_path = deployments_json_path(home);
    fs::create_dir_all(json_path.parent().unwrap()).unwrap();
    let arr: Vec<Value> = deployments
        .iter()
        .map(|(deployment_id, ip)| {
            serde_json::json!({
                "deployment_id": deployment_id,
                "provider": "manual",
                "provider_vm_id": "",
                "provider_key_id": "",
                "region": "",
                "ip": ip,
                "url": format!("https://{ip}:8080"),
                "manifest_path": "/tmp/murmur.yaml",
                "started_at": "2026-06-03T00:00:00Z",
                "status": "running"
            })
        })
        .collect();
    fs::write(&json_path, serde_json::to_string_pretty(&arr).unwrap()).unwrap();
}

fn write_deployment(home: &std::path::Path, deployment_id: &str, ip: &str) {
    write_deployments(home, &[(deployment_id, ip)]);
}

/// Directory `mur destroy` must remove for `deployment_id` — named after the *full* id, which is
/// what `deploy_state::deploy_keys_dir` builds and what `destroy.rs` passes it.
fn deploy_keys_dir(home: &std::path::Path, deployment_id: &str) -> std::path::PathBuf {
    home.join(".murmur").join("deploy_keys").join(deployment_id)
}

/// Staging counterpart of [`deploy_keys_dir`], likewise keyed by the full id.
fn deploy_staging_dir(home: &std::path::Path, deployment_id: &str) -> std::path::PathBuf {
    home.join(".murmur")
        .join("deploy_staging")
        .join(deployment_id)
}

/// Populate the key and staging directories `mur destroy` is expected to sweep, each holding a
/// file so an empty-directory removal cannot pass for a real one.
fn seed_deployment_dirs(home: &std::path::Path, deployment_id: &str) {
    let keys = deploy_keys_dir(home, deployment_id);
    fs::create_dir_all(&keys).unwrap();
    fs::write(keys.join("id_ed25519"), "-----BEGIN PRIVATE KEY-----\n").unwrap();

    let staging = deploy_staging_dir(home, deployment_id);
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("murmur.yaml"), "name: test-agent\n").unwrap();
}

fn read_deployments(home: &std::path::Path) -> Vec<Value> {
    let raw = fs::read_to_string(deployments_json_path(home)).unwrap();
    serde_json::from_str(&raw).unwrap()
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
    write_deployment(dir.path(), DEPLOYMENT_ID, "1.2.3.4");

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("DEPLOYMENT_ID"), "got: {stdout}");
    assert!(stdout.contains(DEPLOYMENT_ID), "got: {stdout}");
    assert!(stdout.contains("1.2.3.4"), "got: {stdout}");
    assert!(stdout.contains("https://1.2.3.4:8080"), "got: {stdout}");
}

/// Backward compatibility, not a record of the current format: `deployments.json` files written
/// before `job_id` became `deployment_id` are still on real disks, and losing one orphans a live
/// VM from `mur destroy`. `DeploymentRecord` carries `#[serde(alias = "job_id")]` for exactly this
/// case; `deploy_state.rs` pins it at the `serde` level, and this pins it through the compiled
/// binary. The raw `"job_id"` literal below is the point of the test — do not "fix" it to
/// `deployment_id`, and do not copy this fixture as a template for new tests.
#[test]
fn ps_lists_deployment_written_before_the_rename() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    let json_path = deployments_json_path(dir.path());
    fs::create_dir_all(json_path.parent().unwrap()).unwrap();
    let legacy = serde_json::json!([{
        "job_id": "job_019e9d84c3b2a1908f7e6d5c4b3a2910",
        "provider": "manual",
        "provider_vm_id": "",
        "provider_key_id": "",
        "region": "",
        "ip": "1.2.3.4",
        "url": "https://1.2.3.4:8080",
        "manifest_path": "/tmp/murmur.yaml",
        "started_at": "2026-06-03T00:00:00Z",
        "status": "running"
    }]);
    fs::write(&json_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("job_019e9d84c3b2a1908f7e6d5c4b3a2910"),
        "pre-rename record must still list: {stdout}"
    );
    assert!(stdout.contains("1.2.3.4"), "got: {stdout}");
}

#[test]
fn ps_multiple_deployments_shows_all() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    write_deployments(
        dir.path(),
        &[
            (DEPLOYMENT_ID, "10.0.0.1"),
            (OTHER_DEPLOYMENT_ID, "10.0.0.2"),
        ],
    );

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .arg("ps")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(DEPLOYMENT_ID), "got: {stdout}");
    assert!(stdout.contains(OTHER_DEPLOYMENT_ID), "got: {stdout}");
}

// ─── mur destroy ──────────────────────────────────────────────────────────────

/// `mur destroy` accepts an unambiguous prefix but must clean up under the matched record's full
/// id — the directories are named after the full id, so cleaning up under the argument would
/// leave the private key and the uploaded staging tree behind for a deployment that is gone.
#[test]
fn destroy_by_prefix_removes_key_and_staging_dirs() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    write_deployment(dir.path(), DEPLOYMENT_ID, "1.2.3.4");
    seed_deployment_dirs(dir.path(), DEPLOYMENT_ID);

    // Deliberately shorter than the id the directories are named after.
    let prefix = &DEPLOYMENT_ID[..12];
    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args(["destroy", prefix])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "destroy failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !deploy_keys_dir(dir.path(), DEPLOYMENT_ID).exists(),
        "private key directory survived destroy"
    );
    assert!(
        !deploy_staging_dir(dir.path(), DEPLOYMENT_ID).exists(),
        "staging directory survived destroy"
    );
    assert!(
        read_deployments(dir.path()).is_empty(),
        "deployments.json entry survived destroy"
    );
}

/// A prefix matching more than one record is refused outright: nothing is removed from disk and
/// no record is dropped, so the user can retry with a longer prefix.
#[test]
fn destroy_by_ambiguous_prefix_removes_nothing() {
    let dir = tempdir().unwrap();
    enable_deploy_beta(dir.path());
    write_deployments(
        dir.path(),
        &[(AMBIGUOUS_ID_A, "10.0.0.1"), (AMBIGUOUS_ID_B, "10.0.0.2")],
    );
    seed_deployment_dirs(dir.path(), AMBIGUOUS_ID_A);
    seed_deployment_dirs(dir.path(), AMBIGUOUS_ID_B);

    let shared_prefix = &AMBIGUOUS_ID_A[..12];
    assert!(
        AMBIGUOUS_ID_B.starts_with(shared_prefix),
        "fixture ids must share a prefix"
    );

    let out = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", dir.path())
        .args(["destroy", shared_prefix])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ambiguous"),
        "expected ambiguity error, got: {stderr}"
    );

    for id in [AMBIGUOUS_ID_A, AMBIGUOUS_ID_B] {
        assert!(
            deploy_keys_dir(dir.path(), id).exists(),
            "{id} key directory was removed"
        );
        assert!(
            deploy_staging_dir(dir.path(), id).exists(),
            "{id} staging directory was removed"
        );
    }
    assert_eq!(
        read_deployments(dir.path()).len(),
        2,
        "a record was dropped"
    );
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
            "--host",
            "1.2.3.4",
            "--manifest",
            "/nonexistent/path/murmur.yaml",
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
            "--host",
            "1.2.3.4",
            "--manifest",
            manifest.to_str().unwrap(),
            "--workdir",
            "/nonexistent/workdir-xyz",
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
            "--host",
            "1.2.3.4",
            "--manifest",
            manifest.to_str().unwrap(),
            "--mur-binary",
            "/nonexistent/mur-linux",
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
            "--host",
            "1.2.3.4",
            "--manifest",
            manifest.to_str().unwrap(),
            "--env",
            "MISSING_EQUALS",
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
    write_deployment(dir.path(), DEPLOYMENT_ID, "9.9.9.9");

    let arr = read_deployments(dir.path());
    let r = &arr[0];

    for field in &[
        "deployment_id",
        "provider",
        "provider_vm_id",
        "provider_key_id",
        "region",
        "ip",
        "url",
        "manifest_path",
        "started_at",
        "status",
    ] {
        assert!(
            r.get(field).is_some(),
            "deployments.json missing field: {field}"
        );
    }
    assert!(
        r.get("job_id").is_none(),
        "job_id is the pre-rename name; current-format records must not carry it"
    );
}
