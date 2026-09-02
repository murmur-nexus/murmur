//! A native tool built for another platform is refused before it is written, and reported by
//! `mur doctor`, on one shared reading of the same bytes.
//!
//! The two surfaces are asserted to agree by extracting the platform string each one prints
//! rather than by comparing both against a literal: the point of the slice is that there is one
//! classifier with two callers, and a test that hardcodes the answer twice would still pass if
//! they drifted apart.
//!
//! Every foreign binary here is a synthetic header built in the test body from the offsets the
//! classifier reads, and the foreign platform is derived from `current_platform()`, so the suite
//! runs unchanged on either Linux target and on darwin.

#[path = "common/mod.rs"]
mod common;

use std::{collections::HashSet, fs, path::Path, sync::Arc};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
};
use murmur_artifact::{
    current_platform, load_runtime_manifest, ArtifactMeta, ContainmentClass, InferenceConfig,
    InferenceDriver, LocalRegistry, Registry, RuntimeManifest, RuntimeType,
};
use predicates::prelude::*;
use tempfile::{tempdir, TempDir};

const TOOL_NAME: &str = "platform-fixture-tool";
const TOOL_VERSION: &str = "0.1.0";

// ── Synthetic executable headers ─────────────────────────────────────────────

/// A 64-byte little-endian ELF64 header carrying `e_machine`.
fn elf64_le(e_machine: u16) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[0..4].copy_from_slice(b"\x7fELF");
    bytes[4] = 2; // ELFCLASS64
    bytes[5] = 1; // little-endian
    bytes[18..20].copy_from_slice(&e_machine.to_le_bytes());
    bytes
}

/// A 32-byte little-endian thin Mach-O header carrying `cputype`.
fn macho64_le(cputype: i32) -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    bytes[0..4].copy_from_slice(&0xFEED_FACFu32.to_le_bytes());
    bytes[4..8].copy_from_slice(&cputype.to_le_bytes());
    bytes
}

/// An executable image for a platform this host is not, paired with the platform string the
/// classifier will name it as.
///
/// Derived from `current_platform()` rather than fixed, so the case under test is always a real
/// mismatch for the machine running the suite. `None` on a host outside the four platform targets
/// — there is no "other platform" to build against.
fn foreign_binary() -> Option<(&'static str, Vec<u8>)> {
    match current_platform() {
        "linux-x86_64" => Some(("linux-aarch64", elf64_le(0xB7))),
        "linux-aarch64" => Some(("linux-x86_64", elf64_le(0x3E))),
        "darwin-aarch64" => Some(("darwin-x86_64", macho64_le(0x0100_0007))),
        "darwin-x86_64" => Some(("darwin-aarch64", macho64_le(0x0100_000C))),
        _ => None,
    }
}

// ── Artifact and project fixtures ────────────────────────────────────────────

/// The packed `murmur.yaml` of a native tool artifact. `implementation: native` is what marks the
/// artifact as one whose `bin/<name>` payload is a host executable.
fn native_tool_manifest() -> String {
    format!(
        "name: {TOOL_NAME}\nversion: {TOOL_VERSION}\nruntime: tool\nimplementation: native\n\
         description: platform check fixture\n"
    )
}

/// Pack a native tool `.mur.zip` whose `bin/<name>` holds exactly `payload`.
fn native_tool_zip(dir: &Path, payload: &[u8]) -> std::path::PathBuf {
    let payload_path = dir.join("payload.bin");
    fs::write(&payload_path, payload).unwrap();
    common::create_native_tool_zip(
        dir,
        TOOL_NAME,
        TOOL_VERSION,
        native_tool_manifest().as_bytes(),
        &payload_path,
    )
}

/// Publish the artifact into the local store under `home`, the store `stage_session` resolves
/// from. `runtime: Native` here matches what a native publish records; doctor deliberately does
/// not read it (see `check_artifact_platform`).
fn publish_to_home(home: &TempDir, artifact_path: &Path) {
    let registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let meta = ArtifactMeta {
        name: TOOL_NAME.to_string(),
        version: TOOL_VERSION.to_string(),
        runtime: RuntimeType::Native,
        artifact_runtime: "native".to_string(),
        platforms: Vec::new(),
        description: None,
        tags: Vec::new(),
    };
    registry
        .publish(meta, &fs::read(artifact_path).unwrap())
        .unwrap();
}

/// A capsule manifest declaring the fixture tool and nothing else.
fn write_capsule_manifest(project_dir: &Path) -> RuntimeManifest {
    let manifest_path = project_dir.join("murmur.yaml");
    fs::write(
        &manifest_path,
        format!(
            "name: platform-capsule\nversion: 0.1.0\nartifacts:\n  - name: {TOOL_NAME}\n    \
             version: {TOOL_VERSION}\n    runtime: tool\n"
        ),
    )
    .unwrap();
    load_runtime_manifest(&manifest_path).unwrap()
}

/// Minimal inference config so `stage_session` treats the capsule as an agent capsule, which is
/// what permits empty component bytes. The endpoint is never contacted during staging.
fn stub_inference() -> Option<InferenceConfig> {
    Some(InferenceConfig {
        transport: "http".to_string(),
        endpoint: Some("http://localhost:9999".to_string()),
        model: "test-model".to_string(),
        api_key: None,
        driver: Some(InferenceDriver {
            artifact: "dummy-driver".to_string(),
            config: None,
        }),
        command: None,
        compaction: None,
        system_prompt: None,
        system_prompt_file: None,
        system_prompt_artifact: None,
        max_turns: 10,
        max_tokens: None,
    })
}

fn stage_request(project_dir: &Path, manifest: &RuntimeManifest) -> StageRequest {
    let artifacts: Vec<ArtifactRequest> = manifest
        .artifacts
        .iter()
        .map(|artifact| ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        })
        .collect();
    let mut allowlisted_tools = HashSet::new();
    allowlisted_tools.insert(TOOL_NAME.to_string());

    StageRequest {
        manifest_dir: project_dir.to_path_buf(),
        capsule_name: manifest.name.clone(),
        capsule_version: manifest.version.clone(),
        capsule_component_bytes: Vec::new(),
        artifacts,
        allowlisted_tools,
        lock_expectations: None,
        capability_policy: capability_policy_from_runtime_manifest(manifest),
        inference: stub_inference(),
        system_prompt_overridden: false,
        context: None,
        context_id: None,
        resume: None,
        otel_endpoint: None,
        eval_config_json: None,
        case_id: None,
        dataset_id: None,
        lifecycle: None,
        lifecycle_override: None,
        trace: None,
        workdir: Some(project_dir.to_path_buf()),
        bind_addr: "127.0.0.1".to_string(),
        internal_port: None,
        declared_containment_floor: ContainmentClass::Advisory,
        exports: None,
        spawn_grant: None,
    }
}

/// Whether any `tools/<TOOL_NAME>/<TOOL_NAME>` exists anywhere under `root`.
///
/// The session workdir is created under the project directory with a generated session id, so a
/// refusal leaves no path the test can name directly — the whole tree is swept instead.
fn staged_binary_exists_under(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == TOOL_NAME)
                && path.join(TOOL_NAME).is_file()
            {
                return true;
            }
            if staged_binary_exists_under(&path) {
                return true;
            }
        }
    }
    false
}

fn mur_doctor(home: &TempDir, project_dir: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
}

/// The platform `mur doctor` named the binary as, pulled out of its failing line.
fn platform_from_doctor_output(stdout: &str) -> String {
    const PREFIX: &str = "native binary is built for ";
    let start = stdout
        .find(PREFIX)
        .unwrap_or_else(|| panic!("doctor output has no platform mismatch line:\n{stdout}"))
        + PREFIX.len();
    let rest = &stdout[start..];
    let end = rest
        .find(',')
        .unwrap_or_else(|| panic!("doctor mismatch line is not the expected shape:\n{stdout}"));
    rest[..end].to_string()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Scenario: staging refuses the artifact and writes nothing.
#[test]
fn staging_refuses_a_native_binary_built_for_another_platform() {
    let Some((foreign_platform, header)) = foreign_binary() else {
        eprintln!("[SKIP] host platform is not one of the four targets");
        return;
    };

    let home = tempdir().unwrap();
    let staging = tempdir().unwrap();
    let project = tempdir().unwrap();

    let artifact = native_tool_zip(staging.path(), &header);
    publish_to_home(&home, &artifact);
    let manifest = write_capsule_manifest(project.path());

    let registry = Arc::new(LocalRegistry::new(
        home.path().join(".murmur").join("artifacts"),
    ));
    let Err(error) = stage_session(registry, stage_request(project.path(), &manifest)) else {
        panic!("staging a foreign-platform native tool must fail");
    };

    match error {
        capsule_runtime::RuntimeError::NativeBinaryPlatformMismatch {
            name,
            binary_platform,
            host_platform,
        } => {
            assert_eq!(name, TOOL_NAME);
            assert_eq!(binary_platform, foreign_platform);
            assert_eq!(host_platform, current_platform());
        }
        other => panic!("expected NativeBinaryPlatformMismatch, got: {other}"),
    }

    assert!(
        !staged_binary_exists_under(project.path()),
        "a refused native binary must not be written to the session workdir"
    );
}

/// Scenario: `mur doctor` fails that artifact's line rather than printing the host platform on a
/// green one.
#[test]
fn doctor_fails_the_line_for_a_native_binary_built_for_another_platform() {
    let Some((foreign_platform, header)) = foreign_binary() else {
        eprintln!("[SKIP] host platform is not one of the four targets");
        return;
    };

    let home = tempdir().unwrap();
    let staging = tempdir().unwrap();
    let project = tempdir().unwrap();

    write_capsule_manifest(project.path());
    let artifact = native_tool_zip(staging.path(), &header);
    common::install_artifact_to_project(project.path(), &artifact).success();

    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains(&format!("\u{2717}  {TOOL_NAME}@{TOOL_VERSION}")),
        "expected a failing line for the artifact:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "native binary is built for {foreign_platform}, this host is {}",
            current_platform()
        )),
        "failing line must name both platforms:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("Fix: {TOOL_NAME}:")),
        "expected a Fix: line naming the artifact:\n{stdout}"
    );
    assert!(
        !stdout.contains("All checks passed."),
        "doctor must not pass a store it cannot run:\n{stdout}"
    );
}

/// Scenario: both surfaces report the same verdict for the same bytes.
///
/// Asserts on the two strings each surface produced, not on a literal, so the test fails if the
/// staging refusal and doctor ever stop reading the same classifier.
#[test]
fn staging_and_doctor_name_the_same_platform_for_the_same_bytes() {
    let Some((_, header)) = foreign_binary() else {
        eprintln!("[SKIP] host platform is not one of the four targets");
        return;
    };

    let home = tempdir().unwrap();
    let staging = tempdir().unwrap();
    let project = tempdir().unwrap();

    let artifact = native_tool_zip(staging.path(), &header);
    publish_to_home(&home, &artifact);
    let manifest = write_capsule_manifest(project.path());

    let registry = Arc::new(LocalRegistry::new(
        home.path().join(".murmur").join("artifacts"),
    ));
    let Err(error) = stage_session(registry, stage_request(project.path(), &manifest)) else {
        panic!("staging a foreign-platform native tool must fail");
    };
    let staging_platform = match error {
        capsule_runtime::RuntimeError::NativeBinaryPlatformMismatch {
            binary_platform, ..
        } => binary_platform,
        other => panic!("expected NativeBinaryPlatformMismatch, got: {other}"),
    };

    common::install_artifact_to_project(project.path(), &artifact).success();
    let assert = mur_doctor(&home, project.path()).failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert_eq!(
        staging_platform,
        platform_from_doctor_output(&stdout),
        "staging and doctor must classify the same bytes identically"
    );
}

/// Scenario: an unrecognised payload format still stages, and doctor says so rather than claiming
/// a platform it never identified.
///
/// The refusal is built on positive identification. A shell script at `bin/<name>` is a shape that
/// works today, and it has to keep working.
#[test]
fn an_unrecognised_payload_stages_and_doctor_reports_it_unverified() {
    let home = tempdir().unwrap();
    let staging = tempdir().unwrap();
    let project = tempdir().unwrap();

    let artifact = native_tool_zip(staging.path(), b"#!/bin/sh\nexit 0\n");
    publish_to_home(&home, &artifact);
    let manifest = write_capsule_manifest(project.path());

    let registry = Arc::new(LocalRegistry::new(
        home.path().join(".murmur").join("artifacts"),
    ));
    let staged = stage_session(registry, stage_request(project.path(), &manifest))
        .expect("an unidentifiable payload must stage exactly as before");
    assert!(
        staged
            .workdir
            .join("tools")
            .join(TOOL_NAME)
            .join(TOOL_NAME)
            .is_file(),
        "the binary must still be installed into the session workdir"
    );

    common::install_artifact_to_project(project.path(), &artifact).success();
    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains(format!(
            "\u{2713}  {TOOL_NAME}@{TOOL_VERSION}"
        )))
        .stdout(predicate::str::contains("platform unverified"))
        .stdout(predicate::str::contains("All checks passed."));
}

/// Scenario: a skill artifact's green line stops carrying a platform string it never verified.
#[test]
fn doctor_reports_a_platform_independent_artifact_as_such() {
    let home = tempdir().unwrap();
    let staging = tempdir().unwrap();
    let project = tempdir().unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        "name: platform-capsule\nversion: 0.1.0\nartifacts:\n  - name: platform-skill\n    \
         version: 0.1.0\n    runtime: skill\n",
    )
    .unwrap();
    let artifact =
        common::create_skill_artifact(staging.path(), "platform-skill", "0.1.0", "# guidance\n");
    common::install_artifact_to_project(project.path(), &artifact).success();

    mur_doctor(&home, project.path())
        .success()
        .stdout(predicate::str::contains(
            "\u{2713}  platform-skill@0.1.0   platform-independent",
        ))
        .stdout(predicate::str::contains("All checks passed."));
}
