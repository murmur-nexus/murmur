//! Regression: a non-root `mur run` must be able to `execve` an allowlisted `shell.allow` binary.
//!
//! This is the one shell-execution suite that spawns the *real compiled `mur` binary* rather than
//! calling `capsule_runtime::launch_session` in-process. That distinction is the whole point.
//! `main()` calls `capsule_runtime::security::harden_process_dumpable()` as its first statement,
//! which marks the runtime process `dumpable = 0`; a forked child inherits that flag through its
//! `mm`, and the kernel then refuses `/proc/<child>/mem` to any non-root, non-ptrace-capable
//! reader — including the seccomp-notify supervisor thread inside the child's *own parent*. Every
//! notified `execve` therefore failed closed with `EACCES`, surfacing as
//! `Permission denied (os error 13)`. The in-process tests in `shell.rs` never run `main()`, so
//! they never see the non-dumpable state and never caught this.
//!
//! Linux-only by construction: `prctl(2)`/`/proc` do not exist on the other platform targets, and
//! macOS resolves to `EnforcementTier::EnvironmentOnly` with no seccomp-notify supervisor at all.
#![cfg(target_os = "linux")]

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// Emitted by the allowlisted binary and asserted on in the tool result. Deliberately not a
/// substring of anything the runtime itself prints, so its presence can only come from the
/// subprocess's real stdout.
const PROBE_SENTINEL: &str = "murmur-dumpable-probe-ok";

/// The exact text the defect produced: `read_cstr_from_child`'s `/proc/<pid>/mem` open failing
/// with `EACCES`, `classify_and_decide` failing closed to `Decision::Deny`, and the supervisor
/// answering the notification with `-EACCES`.
const DENIED_MARKER: &str = "os error 13";

/// `mur run`'s refusal when the host cannot delegate a cgroup v2 scope — an unrelated,
/// pre-existing launch gate that stops the session before any subprocess is forked.
const CGROUP_REFUSAL: &str = "E-RUN-012";

/// True when this test process runs with euid 0. Root reads any `/proc/<pid>/mem` regardless of
/// the target's `dumpable` flag, so a root runner cannot distinguish the fixed state from the
/// broken one and these tests would report a false pass. Mirrors
/// `escape-conformance/src/host.rs::is_root`'s intent; reads `/proc/self/status` rather than
/// calling `libc::geteuid` so this crate's test suite needs no new dependency.
fn running_as_root() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        // `Uid:` is `real effective saved fs` — the effective uid is field 2.
        .and_then(|uids| uids.split_whitespace().nth(1))
        .map(|euid| euid == "0")
        // No `/proc/self/status` at all: treat as non-root and let the assertions speak.
        .unwrap_or(false)
}

fn tool_call_response(command: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": "toolu_probe",
            "name": "bash",
            "input": {"command": command},
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "probe complete"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

/// A project whose manifest grants `shell.allow: [bash]` — a real, present binary — plus the
/// scripted inference endpoint. `containment_yaml` is spliced into `capabilities:` so the same
/// fixture serves the `advisory` (nothing declared) and `scoped` cases.
fn setup_project(endpoint: &str, containment_yaml: &str) -> (TempDir, TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = format!(
        concat!(
            "name: dumpable-probe\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "{containment_yaml}",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        containment_yaml = containment_yaml,
        endpoint = endpoint,
    );
    fs::write(project.path().join("murmur.yaml"), manifest).unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();
    common::install_artifact_to_project(project.path(), &driver_artifact).success();

    (home, artifacts, project)
}

fn mur(home: &TempDir, project: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project)
        .timeout(Duration::from_secs(180));
    command
}

/// The same `mur` invocation, but inside a transient systemd scope of its own with the cgroup v2
/// controllers delegated to it.
///
/// Needed because `mur run` refuses to launch a subprocess-capable capsule unless it can create a
/// child cgroup to bound the process tree (slice `5cd4e8d`, `E-RUN-012`), and a test harness's own
/// cgroup is normally undelegated. Wrapping only the `mur` process — rather than asking the
/// operator to wrap `cargo test` — is what makes this work: cgroup v2's "no internal processes"
/// rule means controllers cannot be enabled in a cgroup that still holds the test binary itself,
/// so putting `cargo`/the harness in the delegated scope does not help, while giving `mur` a scope
/// where it is the only process does. `--scope` execs the command from `systemd-run` itself, so
/// cwd, environment and exit status all pass straight through.
fn mur_in_delegated_scope(home: &TempDir, project: &Path) -> Command {
    let mur_binary = assert_cmd::cargo::cargo_bin("mur");
    let mut command = Command::new("systemd-run");
    command
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
        ])
        .arg(mur_binary)
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project)
        .timeout(Duration::from_secs(180));
    command
}

/// Whether `systemd-run --user` can create a transient scope here at all. Probed by actually
/// creating one, since a systemd binary on `PATH` says nothing about a reachable user manager.
fn delegated_scope_available() -> bool {
    std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
            "true",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The `bash` tool's result text as the driver reported it back to the model on the second turn.
fn tool_result_text(request: &Value, tool_use_id: &str) -> String {
    let messages = request["messages"].as_array().expect("messages array");
    for message in messages {
        if message["role"].as_str() != Some("user") {
            continue;
        }
        let Some(content) = message["content"].as_array() else {
            continue;
        };
        for block in content {
            if block["type"].as_str() != Some("tool_result")
                || block["tool_use_id"].as_str() != Some(tool_use_id)
            {
                continue;
            }
            if let Some(text) = block["content"].as_str() {
                return text.to_string();
            }
            return block["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        }
    }
    panic!("no tool_result block for {tool_use_id} in {request}");
}

/// Every event the session recorded, read from its own `trace.jsonl`
/// (`<project>/workdir/<session_id>/trace.jsonl`, the CLI's default no-`--workdir` layout).
fn trace_events(project: &Path) -> Vec<Value> {
    let sessions = fs::read_dir(project.join("workdir"))
        .expect("mur run must create a workdir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("trace.jsonl").is_file())
        .collect::<Vec<PathBuf>>();
    assert_eq!(
        sessions.len(),
        1,
        "expected exactly one traced session dir, found {sessions:?}"
    );

    fs::read_to_string(sessions[0].join("trace.jsonl"))
        .expect("trace.jsonl must exist after a session")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("every trace line is valid JSON"))
        .collect()
}

fn events_of_type<'a>(events: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// Drives one full `mur run` and asserts the allowlisted binary actually executed.
fn assert_allowlisted_exec_succeeds(containment_yaml: &str) {
    let server = common::ScriptedServer::start(vec![
        tool_call_response(&format!("echo {PROBE_SENTINEL}")),
        end_turn_response(),
    ]);
    let (home, _artifacts, project) = setup_project(&server.endpoint, containment_yaml);

    let run_args = [
        "run",
        "--manifest",
        "murmur.yaml",
        "--task",
        "Run the probe command and report its output.",
    ];

    let mut output = mur(&home, project.path())
        .args(run_args)
        .assert()
        .get_output()
        .clone();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Pre-existing, unrelated launch gate (slice `5cd4e8d`): a capsule that can spawn native
    // subprocesses refuses to launch unless a cgroup v2 scope can bound the process tree, and the
    // cgroup a test harness runs in is normally undelegated. Retry with `mur` in a delegated scope
    // of its own; there is nothing to observe about *this* slice until it launches.
    if !output.status.success() && stderr.contains(CGROUP_REFUSAL) && delegated_scope_available() {
        fs::remove_dir_all(project.path().join("workdir")).ok();
        output = mur_in_delegated_scope(&home, project.path())
            .args(run_args)
            .assert()
            .get_output()
            .clone();
        stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    }

    if !output.status.success() && stderr.contains(CGROUP_REFUSAL) {
        eprintln!(
            "skipping: this host cannot delegate a cgroup v2 scope to `mur`, so `mur run` refuses \
             with {CGROUP_REFUSAL} before spawning any subprocess — no execve happens and this \
             slice's behaviour is unobservable here"
        );
        return;
    }
    assert!(
        output.status.success(),
        "`mur run` must complete the scripted session: {stderr}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected a tool turn and a final turn");
    let result = tool_result_text(&requests[1], "toolu_probe");

    assert!(
        !result.contains(DENIED_MARKER),
        "the allowlisted binary was denied by the seccomp-notify supervisor — the forked child \
         is still non-dumpable, so `/proc/<pid>/mem` is unreadable to its own parent: {result}"
    );
    assert!(
        result.contains(PROBE_SENTINEL),
        "the tool result must carry the binary's real stdout: {result}"
    );

    let events = trace_events(project.path());

    // The `tool_call` event is where the defect showed up as `status: "error"`.
    let tool_calls = events_of_type(&events, "tool_call");
    assert_eq!(tool_calls.len(), 1, "expected one tool_call event: {tool_calls:?}");
    assert_eq!(
        tool_calls[0]["status"], "ok",
        "trace.jsonl must record the tool call as ok: {}",
        tool_calls[0]
    );

    // And the `shell` event proves the subprocess itself ran to completion, rather than the tool
    // call merely reporting an error string as a successful result.
    let shell_events = events_of_type(&events, "shell");
    assert_eq!(shell_events.len(), 1, "expected one shell event: {shell_events:?}");
    assert_eq!(
        shell_events[0]["exit_code"], 0,
        "the allowlisted binary must exit 0: {}",
        shell_events[0]
    );
}

/// Whether this host can actually provide `scoped` (Landlock ABI usable —
/// `EnforcementTier::KernelFull`), asked of the binary itself via the read-only
/// `--explain-scope --json` report so the answer comes from the same probe `mur run` uses.
fn host_provides_scoped(home: &TempDir, project: &Path) -> bool {
    let output = mur(home, project)
        .args([
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    report["floor_met"] == json!(true)
}

/// Scenario 1: non-root, containment `advisory` (nothing declared — the default class every host
/// satisfies). The tier is host-probed and the seccomp-notify filter is installed independently
/// of the declared containment class.
#[test]
fn advisory_containment_executes_an_allowlisted_shell_binary() {
    if running_as_root() {
        eprintln!(
            "skipping: test runner is euid 0, which reads any /proc/<pid>/mem regardless of the \
             target's dumpable flag — this test cannot distinguish fixed from broken as root"
        );
        return;
    }

    assert_allowlisted_exec_succeeds("");
}

/// Scenario 2: non-root, containment `scoped`. Skipped (not failed) on a host whose kernel cannot
/// provide `scoped` at all, where `mur run` correctly refuses with `E-CAP-003` for its own,
/// unrelated reason.
#[test]
fn scoped_containment_executes_an_allowlisted_shell_binary() {
    if running_as_root() {
        eprintln!(
            "skipping: test runner is euid 0, which reads any /proc/<pid>/mem regardless of the \
             target's dumpable flag — this test cannot distinguish fixed from broken as root"
        );
        return;
    }

    let containment_yaml = "  containment: scoped\n";

    // A throwaway project, only to ask the binary whether this host meets the `scoped` floor.
    let probe_server = common::ScriptedServer::start(Vec::new());
    let (probe_home, _probe_artifacts, probe_project) =
        setup_project(&probe_server.endpoint, containment_yaml);
    if !host_provides_scoped(&probe_home, probe_project.path()) {
        eprintln!(
            "skipping: this host does not meet the `scoped` floor (no usable Landlock ABI), so \
             `mur run` refuses with E-CAP-003 before reaching any execve"
        );
        return;
    }
    drop(probe_project);

    assert_allowlisted_exec_succeeds(containment_yaml);
}
