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
        .timeout(RUN_TIMEOUT)
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--verbose",
        ])
        .assert()
}

/// Every line of the one `trace.jsonl` the run left under `home`'s records.
fn trace_lines(home: &Path) -> Vec<serde_json::Value> {
    let mut stack = vec![home.join(".murmur").join("records")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
                return fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            }
        }
    }
    panic!("no trace.jsonl under {}", home.display());
}

/// The first and last lines of the one `trace.jsonl` the run left under `home`'s records.
fn trace_frame(home: &Path) -> (serde_json::Value, serde_json::Value) {
    let lines = trace_lines(home);
    (lines[0].clone(), lines.last().unwrap().clone())
}

/// Whether a file named `name` exists anywhere under `root`.
fn file_under(root: &Path, name: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file| file == name) {
                return true;
            }
        }
    }
    false
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

    let (start, end) = trace_frame(home.path());
    assert_eq!(start["event_type"], "session_start");
    assert_eq!(end["event_type"], "session_end");
    assert_eq!(end["exit_status"], "deadline_exceeded");
    assert!(
        !trace_lines(home.path())
            .iter()
            .any(|line| line["event_type"] == "call_deadline"),
        "computing past the deadline is the epoch's to stop, and writes no call_deadline"
    );
}

/// A capsule whose `run` sleeps — a host wait the epoch cannot interrupt — is stopped at its
/// deadline plus the grace instead of sleeping out its eight seconds: the run fails as a deadline,
/// one `call_deadline` names the stopped `run`, and the marker the finished run would write is
/// never written.
#[test]
fn a_capsule_sleeping_in_the_host_is_stopped_at_its_deadline() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest_path = create_project(
        project.path(),
        "capsule-sleep-long.wasm",
        "  limits:\n    deadline_seconds: 2\n",
    );

    run_capsule(home.path(), &manifest_path)
        .failure()
        .stderr(predicate::str::contains("E-RUN-001"))
        .stderr(predicate::str::contains(
            "capsule execution exceeded its 2s deadline",
        ));

    let lines = trace_lines(home.path());
    let cuts: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["event_type"] == "call_deadline")
        .collect();
    assert_eq!(cuts.len(), 1, "{lines:#?}");
    let cut = cuts[0];
    assert_eq!(cut["role"], "capsule");
    assert_eq!(cut["component"], "capsule");
    assert_eq!(cut["call"], "run");
    assert_eq!(cut["deadline_seconds"], 2);
    let elapsed = cut["elapsed_ms"].as_u64().unwrap();
    assert!((2_000..8_000).contains(&elapsed), "{cut}");
    let end = lines.last().unwrap();
    assert_eq!(end["event_type"], "session_end");
    assert_eq!(end["exit_status"], "deadline_exceeded");
    assert!(
        !file_under(project.path(), "done.txt") && !file_under(home.path(), "done.txt"),
        "the stopped run wrote nothing after its sleep"
    );
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

    let (start, end) = trace_frame(home.path());
    assert_eq!(start["event_type"], "session_start");
    assert_eq!(end["event_type"], "session_end");
    assert_eq!(end["exit_status"], "resource_limit");
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
