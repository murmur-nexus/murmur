//! What a host claims about containment, measured against what it then does.
//!
//! Three properties, each exercised rather than inferred: that `sealed` survives the first
//! subprocess, that a declared `io.max` ceiling is actually on the session's cgroup scope, and
//! that the two reports an operator reads — `--explain-scope --json` and
//! `session_start.effective_grants` — agree with the launch that produced them.
//!
//! Every assertion here asks the host first and asserts the branch that host is actually in,
//! following `containment.rs`'s rule: `sealed` is genuinely achievable on a correctly configured
//! Linux box, so a test that hardcodes either answer passes on CI and fails on the machine the
//! feature was built for.

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// A capsule that talks to `endpoint`, may run `binary`, and declares `containment`.
fn write_agent_project(
    project_dir: &Path,
    endpoint: &str,
    containment: &str,
    binary: &str,
) -> PathBuf {
    let manifest = project_dir.join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: sealed-io-fixture\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_ANTHROPIC_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             \x20   gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             capabilities:\n  containment: {containment}\n  network:\n    allow:\n      - {endpoint}\n\
             \x20 shell:\n    allow:\n      - {binary}\n\
             inference:\n  transport: http\n  model: test-model\n  \
             driver:\n    artifact: {DRIVER_ANTHROPIC_NAME}\n"
        ),
    )
    .unwrap();
    manifest
}

/// Publishes the anthropic driver into `home`'s local registry, which every capsule here needs.
fn publish_driver(home: &TempDir) {
    let artifact_dir = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &artifact).success();
}

/// One scripted `tool_use` calling the `binary` tool with `command`, then a plain answer.
///
/// The tool name has to be the allowlisted binary: a capsule declaring `shell.allow: [echo]` is
/// offered exactly that tool, and any other name is refused by the dispatcher without a subprocess
/// ever being spawned — which would leave a scenario asserting nothing about the mechanism.
/// `command` is the argument list alone, without the binary name.
fn call_then_answer(binary: &str, command: &str) -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_call",
                "name": binary,
                "input": {"command": command}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        answer(),
    ]
}

fn answer() -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "Done."}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn mur_run(home: &TempDir, manifest: &Path, task: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            task,
            "--verbose",
        ])
        .assert()
}

/// The `session_start` event's `effective_grants`, read out of the trace the run wrote.
#[cfg(target_os = "linux")]
fn session_start_grants(home: &TempDir, project_dir: &Path) -> Value {
    let trace_path = common::newest_session_records(home.path(), &project_dir.join("workdir"))
        .expect("the session wrote a records directory")
        .join("trace.jsonl");
    let trace = fs::read_to_string(trace_path).unwrap();
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "session_start")
        .expect("every session writes a session_start")["effective_grants"]
        .clone()
}

// ── Scenario A: a sealed verdict survives the first subprocess ────────────────

/// One session of a capsule declaring `containment` that runs `ls /usr/lib` once: its
/// `--explain-scope --json`, the tool result the provider received, and its `session_start`.
#[cfg(target_os = "linux")]
fn ls_usr_lib_under(containment: &str) -> (Value, String, Value) {
    let server = ScriptedServer::start(call_then_answer("ls", "/usr/lib"));
    let home = TempDir::new().unwrap();
    publish_driver(&home);
    let project = TempDir::new().unwrap();
    let manifest = write_agent_project(project.path(), &server.endpoint, containment, "ls");

    let report = common::explain_scope_json(&home, &manifest);
    mur_run(&home, &manifest, "List /usr/lib.").success();
    let tool_result = common::tool_result_text(&server.requests(), "toolu_call")
        .expect("the provider received the ls call's result");

    let trace_path = common::newest_session_records(home.path(), &project.path().join("workdir"))
        .expect("the session wrote a records directory")
        .join("trace.jsonl");
    let session_start = fs::read_to_string(trace_path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["event_type"] == "session_start")
        .expect("every session writes a session_start");
    (report, tool_result, session_start)
}

/// The exit code a shell tool result reports, from its `Exit code: N` line.
#[cfg(target_os = "linux")]
fn exit_code(tool_result: &str) -> i64 {
    tool_result
        .lines()
        .find_map(|line| line.trim().strip_prefix("Exit code: "))
        .and_then(|code| code.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `Exit code:` line in {tool_result}"))
}

/// On a host that can back `sealed`, a capsule declaring `scoped` reports the class and tier its
/// commands run on — `scoped` on `landlock+seccomp` — in `--explain-scope --json` and in
/// `session_start`, and its `ls /usr/lib` is denied by Landlock. A capsule declaring `sealed` on
/// the same host reports `sealed` on the composed root, where `/usr/lib` is bound and listable.
/// Both report the host can back `sealed`.
#[cfg(target_os = "linux")]
#[test]
fn a_scoped_capsule_on_a_sealed_host_reports_and_runs_on_landlock() {
    let test = "a_scoped_capsule_on_a_sealed_host_reports_and_runs_on_landlock";
    if common::skip_without_host_support(test) {
        return;
    }
    let host =
        capsule_runtime::detect_containment_decision(murmur_artifact::ContainmentClass::Sealed);
    if host.0.achieved() != murmur_artifact::ContainmentClass::Sealed {
        eprintln!(
            "[SKIP-HOST] {test}: this host cannot back sealed ({})",
            host.1
                .map(|shortfall| shortfall.reason())
                .unwrap_or_else(|| format!("it reaches {}", host.0.achieved()))
        );
        return;
    }

    let (report, tool_result, session_start) = ls_usr_lib_under("scoped");
    assert_eq!(report["achieved_containment"], "scoped", "{report}");
    assert_eq!(report["enforcement_tier"], "landlock+seccomp", "{report}");
    assert_eq!(report["host_containment"], "sealed", "{report}");
    assert_eq!(
        report["filesystem_boundary"]["restriction"], "enforced",
        "{report}"
    );
    assert_ne!(exit_code(&tool_result), 0, "{tool_result}");
    assert!(
        tool_result.contains("Permission denied"),
        "Landlock denies listing /usr/lib to a scoped capsule: {tool_result}"
    );
    assert_eq!(
        session_start["containment_achieved"], "scoped",
        "{session_start}"
    );
    let grants = &session_start["effective_grants"];
    for key in [
        "achieved_containment",
        "enforcement_tier",
        "host_containment",
    ] {
        assert_eq!(grants[key], report[key], "{key}: {grants}");
    }

    let (report, tool_result, session_start) = ls_usr_lib_under("sealed");
    assert_eq!(report["achieved_containment"], "sealed", "{report}");
    assert_eq!(
        report["enforcement_tier"], "mountns+pivot_root+landlock+seccomp",
        "{report}"
    );
    assert_eq!(report["host_containment"], "sealed", "{report}");
    assert_eq!(
        report["filesystem_boundary"]["restriction"], "absent",
        "{report}"
    );
    assert_eq!(exit_code(&tool_result), 0, "{tool_result}");
    // The `Stdout:` section is present only when `ls` printed something.
    let listing: Vec<&str> = tool_result
        .lines()
        .skip_while(|line| *line != "Stdout:")
        .skip(1)
        .take_while(|line| *line != "Stderr:")
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(
        !listing.is_empty(),
        "the composed root binds /usr and lists it: {tool_result}"
    );
    assert_eq!(
        session_start["containment_achieved"], "sealed",
        "{session_start}"
    );
    let grants = &session_start["effective_grants"];
    for key in [
        "achieved_containment",
        "enforcement_tier",
        "host_containment",
    ] {
        assert_eq!(grants[key], report[key], "{key}: {grants}");
    }
}

/// The probe rehearses the whole composed-root shape through `pivot_root(2)`, so the launch
/// verdict and the first subprocess cannot disagree.
///
/// On a host reporting `sealed`, the run reaches the agent loop, the `echo` call executes, and no
/// `E-RUN-014` appears. On a host reporting anything below it, the refusal is `E-CAP-003`, it
/// names the specific missing mechanism, and the scripted provider recorded **zero** requests —
/// the refusal is ahead of every provider call, so the surprise costs nothing.
#[test]
fn a_sealed_verdict_survives_the_first_subprocess() {
    if common::skip_without_host_support("a_sealed_verdict_survives_the_first_subprocess") {
        return;
    }
    let server = ScriptedServer::start(call_then_answer("echo", "sealed-subprocess-ran"));
    let home = TempDir::new().unwrap();
    publish_driver(&home);

    let project = TempDir::new().unwrap();
    let manifest = write_agent_project(project.path(), &server.endpoint, "sealed", "echo");

    let report = common::explain_scope_json(&home, &manifest);
    let achieved = report["achieved_containment"].as_str().unwrap().to_string();

    if achieved == "sealed" {
        let assertion = mur_run(&home, &manifest, "Echo something.").success();
        let stderr = String::from_utf8(assertion.get_output().stderr.clone()).unwrap();
        assert!(
            !stderr.contains("E-RUN-014"),
            "a host the probe passed must not fail composing the root:\n{stderr}"
        );
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            2,
            "the run reached the agent loop and answered the tool result"
        );
        // The subprocess itself, not just the turn around it: the echoed marker can only reach
        // the provider by way of a process that ran inside the composed root. Asserting the turn
        // count alone passes on a tool call the dispatcher refused before spawning anything.
        // Read by whole lines of the result, so the command text in the call's own input cannot
        // stand in for the output.
        let tool_result = common::tool_result_text(&requests, "toolu_call")
            .expect("the provider received the echo call's result");
        assert!(
            tool_result
                .lines()
                .any(|line| line == "sealed-subprocess-ran")
                && tool_result.lines().next() == Some("Exit code: 0"),
            "the echo subprocess must have run and its output reached the provider: {tool_result}"
        );
        return;
    }

    let assertion = mur_run(&home, &manifest, "Echo something.").failure();
    let stderr = String::from_utf8(assertion.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-CAP-003"), "stderr was:\n{stderr}");
    // Derived from the enum, never a hand-written list: a blocker added later cannot make this
    // assertion lie about the host.
    assert!(
        capsule_runtime::sealed::SealedBlocker::ALL
            .iter()
            .any(|blocker| stderr.contains(&blocker.reason())),
        "the refusal must name one specific mechanism, got:\n{stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "a refused launch must not have paid for an inference call"
    );
}

// ── Scenarios C and D: a declared io.max ceiling either applies or says it did not ──

/// What one run reports about `io.max`, in all three places the property is claimed.
#[cfg(target_os = "linux")]
struct IoMaxObservation {
    explained: Value,
    session_start: Value,
    stderr: String,
}

#[cfg(target_os = "linux")]
fn run_and_observe(
    project_dir: &Path,
    home: &TempDir,
    server: &ScriptedServer,
) -> IoMaxObservation {
    let manifest = write_agent_project(project_dir, &server.endpoint, "advisory", "echo");
    let explained = common::explain_scope_json(home, &manifest)["io_max"].clone();
    let assertion = mur_run(home, &manifest, "Say something.").success();
    IoMaxObservation {
        explained,
        session_start: session_start_grants(home, project_dir)["io_max"].clone(),
        stderr: String::from_utf8(assertion.get_output().stderr.clone()).unwrap(),
    }
}

#[cfg(target_os = "linux")]
fn warning_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("warning[W-SEC-021]"))
        .collect()
}

/// A project on a tmpfs cannot have an `io.max` ceiling — the block layer accepts no `MAJ:MIN` for
/// it — and one on the checkout's own block-device-backed filesystem can. The two runs differ in
/// exactly that field, in the diagnostic, on stderr, and in the trace.
///
/// `/dev/shm` is tmpfs on every ordinary Linux host: no privilege, no mount, no container needed.
///
/// On a host that cannot delegate a cgroup scope there is nothing to observe — the launch refuses
/// with `E-RUN-012` before a scope exists — and the same ground is covered by `cgroup.rs`'s unit
/// tests over `IoMaxReport` and by
/// `docs/content/reference/resource-limits-manual-verification.md`.
#[test]
#[cfg(target_os = "linux")]
fn a_failed_io_max_write_is_visible_wherever_the_ceiling_is_claimed() {
    if common::skip_without_host_support(
        "a_failed_io_max_write_is_visible_wherever_the_ceiling_is_claimed",
    ) {
        return;
    }
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("[SKIP-HOST] this host has no /dev/shm tmpfs to place a project on");
        return;
    }

    let home = TempDir::new().unwrap();
    publish_driver(&home);

    // Scenario C: tmpfs. Scenario D: the checkout's own filesystem, which `CARGO_TARGET_TMPDIR`
    // always sits on.
    let on_tmpfs = tempfile::Builder::new().tempdir_in(shm).unwrap();
    let on_disk = tempfile::Builder::new()
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();

    let tmpfs_server = ScriptedServer::start(vec![answer()]);
    let tmpfs = run_and_observe(on_tmpfs.path(), &home, &tmpfs_server);
    let disk_server = ScriptedServer::start(vec![answer()]);
    let disk = run_and_observe(on_disk.path(), &home, &disk_server);

    // The declared ceiling is reported whatever became of it, and it is the same ceiling on both.
    assert_eq!(
        tmpfs.explained["declared_bytes_per_sec"],
        disk.explained["declared_bytes_per_sec"]
    );
    assert!(tmpfs.explained["declared_bytes_per_sec"].as_u64().unwrap() > 0);

    // The diagnostic and the trace agree with each other, per run — that is the property, and it
    // holds whichever way this host answers.
    for observation in [&tmpfs, &disk] {
        assert_eq!(
            observation.explained["status"], observation.session_start["status"],
            "--explain-scope and session_start disagreed: {:?} vs {:?}",
            observation.explained, observation.session_start
        );
        let fired = warning_lines(&observation.stderr);
        for line in &fired {
            // The exact text an operator reads, pinned here so the docs page and the emission
            // cannot drift apart: what did not apply, what is still enforced, and the ceiling.
            // The manifest declares no `cgroup_cpu_percent`, so the scope has no `cpu.max`.
            assert!(
                line.contains(
                    "the declared capabilities.resources.cgroup_io_bytes_per_sec ceiling did \
                     not apply to this session's cgroup scope"
                ) && line.contains("memory.max and pids.max are still enforced on the scope")
                    && !line.contains("cpu.max")
                    && line.contains("bytes/s)")
                    && line.contains("#w-sec-021"),
                "unexpected W-SEC-021 text: {line}"
            );
        }
        let fired = fired.len();
        let expected = usize::from(observation.session_start["status"] == "unavailable");
        assert_eq!(
            fired, expected,
            "W-SEC-021 must fire exactly once for an unapplied ceiling and never otherwise, \
             status was {:?}:\n{}",
            observation.session_start["status"], observation.stderr
        );
    }

    assert_eq!(
        tmpfs.session_start["status"], "unavailable",
        "a tmpfs has no block device for io.max to name: {:?}",
        tmpfs.session_start
    );
    assert!(
        tmpfs.session_start["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "an unapplied ceiling must say why: {:?}",
        tmpfs.session_start
    );
    assert_eq!(
        tmpfs.explained["reason"], tmpfs.session_start["reason"],
        "the diagnostic and the trace must carry the same reason, word for word"
    );

    // `enforced` needs a filesystem with a block device behind it, which tmpfs, overlayfs and
    // every FUSE mount lack — a checkout on one of those has no device for the block layer to
    // bind a ceiling to, however well the host delegates cgroups. The gate asks the resolution a
    // launch performs rather than re-deriving it here, so this stands down only where the launch
    // would also find no device. The property above — the three surfaces agree, and W-SEC-021
    // fires exactly for `unavailable` — is what the run proves in that case, and
    // `docs/content/reference/resource-limits-manual-verification.md` covers the rest.
    if !capsule_runtime::io_max_device_available(on_disk.path()) {
        eprintln!(
            "[SKIP-HOST] a_failed_io_max_write_is_visible_wherever_the_ceiling_is_claimed: the \
             checkout sits on a filesystem with no block device behind it, so the `enforced` half \
             of this scenario cannot be observed here"
        );
        return;
    }
    assert_eq!(
        disk.session_start["status"], "enforced",
        "a block-device-backed filesystem accepts the io.max write: {:?}",
        disk.session_start
    );
    assert!(
        disk.session_start.get("reason").is_none() || disk.session_start["reason"].is_null(),
        "an applied ceiling has nothing to explain: {:?}",
        disk.session_start
    );
}

/// The key is always present, on the same terms `workdir_exec` is: an absent `io_max` identifies a
/// runtime that predates the field, not a host that was not asked.
#[test]
fn explain_scope_always_carries_an_io_max_object() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: io-max-fixture\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::write(project.path().join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();

    let report = common::explain_scope_json(&home, &project.path().join("murmur.yaml"));
    let io_max = &report["io_max"];
    assert!(
        io_max.is_object(),
        "io_max must always be an object: {report}"
    );
    assert!(io_max["declared_bytes_per_sec"].is_u64());
    assert!(matches!(
        io_max["status"].as_str(),
        Some("enforced" | "unavailable" | "not-required" | "not-probed")
    ));
    // A capsule that can reach no native subprocess is given no scope at launch, so the
    // diagnostic must not exercise a write the launch would never perform.
    assert_eq!(io_max["status"], "not-required");
}
