//! End-to-end coverage for `capabilities.limits`: the epoch deadline, the store resource
//! limiter, and manifest-time validation of the block itself.
//!
//! Every `mur run` here is wrapped in an `assert_cmd` timeout well above the limit under
//! test. That timeout is the regression guard: if the deadline or the limiter stops firing,
//! these tests fail on the timeout instead of hanging the suite forever — which is exactly
//! the failure mode epoch interruption exists to remove.

use std::{fs, path::Path, path::PathBuf, time::Duration};

use assert_cmd::Command;
use predicates::prelude::*;

/// Generous relative to every limit asserted below (2s deadline, 16 MiB cap), so tripping
/// it means the guest was never bounded at all rather than merely being slow.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

/// Writes a minimal capsule project with the given `capabilities:` body.
///
/// Deliberately omits `artifacts:` entirely rather than emitting an empty key: these
/// fixtures call no tools, and an `artifacts:` key with nothing under it parses as YAML
/// null rather than an empty list.
fn create_project(project_dir: &Path, capsule_fixture: &str, capabilities_yaml: &str) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        format!("name: capsule\nversion: 0.0.1\ncapabilities:\n{capabilities_yaml}"),
    )
    .unwrap();

    fs::copy(
        fixture_component(capsule_fixture),
        project_dir.join("capsule.wasm"),
    )
    .unwrap();

    project_dir.join("murmur.yaml")
}

fn run_capsule(home: &Path, manifest_path: &Path) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .timeout(RUN_TIMEOUT)
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--verbose",
        ])
        .assert()
}

/// The headline proof: a capsule spinning in a loop that never returns and never calls the
/// host is interrupted at its deadline and `mur run` exits. Without epoch interruption this
/// command could not be made to terminate.
#[test]
fn spin_loop_capsule_is_interrupted_at_its_configured_deadline() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_project(
        project.path(),
        "capsule-spin-loop.wasm",
        "  limits:\n    deadline_seconds: 2\n",
    );

    run_capsule(home.path(), &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-001"))
        // Distinguishable from a guest panic, which would say "capsule execution trapped".
        .stderr(predicate::str::contains(
            "capsule execution exceeded its 2s deadline",
        ))
        .stderr(predicate::str::contains(
            "capabilities.limits.deadline_seconds",
        ));
}

/// A capsule that grows its linear memory without bound traps at the configured cap rather
/// than consuming host memory until something else gives out.
#[test]
fn memory_balloon_capsule_traps_at_its_configured_memory_cap() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // 16 MiB: far above what the fixture needs to start, far below what its 8 MiB-per-chunk
    // retention loop reaches within a few iterations.
    let manifest_path = create_project(
        project.path(),
        "capsule-memory-balloon.wasm",
        "  limits:\n    memory_bytes: 16777216\n",
    );

    run_capsule(home.path(), &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-001"))
        .stderr(predicate::str::contains(
            "exceeded its configured resource limits",
        ))
        .stderr(predicate::str::contains("capabilities.limits.memory_bytes"));
}

/// A nonsensical limit is a manifest authoring error, so it must be rejected while parsing
/// — naming the field — rather than surfacing later as an opaque trap from a guest that
/// could never have run.
#[test]
fn zero_memory_bytes_is_rejected_at_manifest_parse_time() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Paired with the spin-loop fixture on purpose: if this were *not* rejected at parse
    // time, the run would proceed to execute a guest that never returns.
    let manifest_path = create_project(
        project.path(),
        "capsule-spin-loop.wasm",
        "  limits:\n    memory_bytes: 0\n",
    );

    run_capsule(home.path(), &manifest_path)
        .failure()
        .stderr(predicate::str::contains("capabilities.limits.memory_bytes"))
        .stderr(predicate::str::contains("must be greater than zero"));
}

/// The limits are opt-in: a manifest with no `capabilities.limits` block still runs a
/// normal capsule to completion, proving the built-in defaults are not so tight that they
/// trip ordinary work.
#[test]
fn capsule_without_a_limits_block_runs_normally_under_default_limits() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // `capsule-filesystem-escape` writes a file and returns — no tools, no network.
    fs::write(
        project.path().join("murmur.yaml"),
        "name: capsule\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-filesystem-escape.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    run_capsule(home.path(), &project.path().join("murmur.yaml"))
        .success()
        .stdout(predicate::str::contains("status:  ok"));
}
