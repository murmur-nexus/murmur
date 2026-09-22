//! The decision point on `transport: process`: what a policy hook and a `read_only` declaration
//! do to a tool call that arrives over the tool bridge.
//!
//! The counterpart to `deny.rs` and `protected_paths.rs`, which prove the same two refusals on
//! `transport: http`. Everything here runs a real fake harness against a real bridge: the hook
//! components are hand-authored WAT compiled in-test, so nothing depends on a `default-artifacts`
//! checkout and no case is `#[ignore]`d.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use capsule_runtime::launch_session;
use common::hook_wat::{create_hook_zip, deny_hook_wasm, none_hook_wasm, spin_hook_wasm};
use serde_json::Value;

const DRIVER: &str = "fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The native tool every hook scenario calls through the bridge. It writes [`MARKER`] and answers
/// with [`TOOL_OUTPUT`], so a refusal is measured twice: by the file that is not there and by the
/// string that appears nowhere in the run.
const TOOL: &str = "marker-tool";
const MARKER: &str = "tool-ran.txt";
const TOOL_OUTPUT: &str = "MARKER-TOOL-RAN";

/// The prebuilt WASM tool the timing case calls, which echoes its input and touches no disk.
const ECHO_TOOL: &str = "echo-tool";

/// The file `read_only` protects, and the subtree that covers it.
const PROTECTED_FILE: &str = "tests/api.py";
const PROTECTED_RULE: &str = "tests";
const PROTECTED_CONTENTS: &str = "assert compute() == 42\n";

fn marker_tool_script() -> String {
    format!(
        "#!/bin/sh\n: > {MARKER}\necho '{}'\n",
        format_args!(
            r#"{{"status":"passed","summary":"ok","data":"{TOOL_OUTPUT}","data_path":null,"truncated":false,"metadata":[]}}"#
        )
    )
}

/// Pack the prebuilt echo component as a `runtime: wasm` tool artifact.
fn create_wasm_tool_artifact(dir: &Path, name: &str, version: &str) -> PathBuf {
    use std::io::Write;
    use zip::{
        write::{FileOptions, SimpleFileOptions},
        CompressionMethod, ZipWriter,
    };

    let path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("run/components/echo-tool.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    path
}

/// One hook to publish and declare: manifest name, `binding:`, `commit_policy:`, component.
struct Hook<'a> {
    name: &'a str,
    binding: &'a str,
    commit_policy: &'a str,
    wasm: Vec<u8>,
}

fn hook<'a>(name: &'a str, binding: &'a str, commit_policy: &'a str, wasm: Vec<u8>) -> Hook<'a> {
    Hook {
        name,
        binding,
        commit_policy,
        wasm,
    }
}

/// What a scenario's capsule declares, and what its harness is told to ask the bridge for.
struct Capsule<'a> {
    hooks: Vec<Hook<'a>>,
    /// `true` for the native [`TOOL`], which is published and declared when it is.
    marker_tool: bool,
    /// `true` for the prebuilt [`ECHO_TOOL`].
    echo_tool: bool,
    read_only: &'a [&'a str],
    shell_allow: &'a [&'a str],
    deadline_seconds: Option<u64>,
    /// The fake harness profile, and the three variables that tell it what to call.
    profile: &'a str,
    call_tool: Option<&'a str>,
    call_args: Option<&'a str>,
    calls: u32,
}

impl<'a> Capsule<'a> {
    /// A capsule whose harness makes one bridged call to the native [`TOOL`].
    fn calling_the_marker_tool() -> Self {
        Self {
            hooks: Vec::new(),
            marker_tool: true,
            echo_tool: false,
            read_only: &[],
            shell_allow: &[],
            deadline_seconds: None,
            profile: "bridge-call",
            call_tool: Some(TOOL),
            call_args: Some(r#"{"msg":"ping-7"}"#),
            calls: 1,
        }
    }

    fn with_hook(mut self, hook: Hook<'a>) -> Self {
        self.hooks.push(hook);
        self
    }

    fn deadline_seconds(mut self, seconds: u64) -> Self {
        self.deadline_seconds = Some(seconds);
        self
    }
}

/// What one launched session left behind.
struct Session {
    trace: Vec<Value>,
    trace_raw: String,
    workdir: PathBuf,
    _project: tempfile::TempDir,
}

impl Session {
    fn events(&self, event_type: &str) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|e| e["event_type"] == event_type)
            .collect()
    }

    /// The one `call_denied` line, with the whole trace in the failure message — a refusal that
    /// did not get recorded is the defect these tests exist to catch.
    fn denial(&self) -> &Value {
        let denials = self.events("call_denied");
        assert_eq!(
            denials.len(),
            1,
            "expected exactly one call_denied line; trace was:\n{}",
            self.trace_raw
        );
        denials[0]
    }

    /// The one `protected_path_denied` line, on the same terms.
    fn refusal(&self) -> &Value {
        let refusals = self.events("protected_path_denied");
        assert_eq!(
            refusals.len(),
            1,
            "expected exactly one protected_path_denied line; trace was:\n{}",
            self.trace_raw
        );
        refusals[0]
    }

    fn marker_exists(&self) -> bool {
        self.workdir.join(MARKER).exists()
    }

    /// What the harness was handed by the bridge, which it reports back as its result — the
    /// refusal text verbatim for every scenario where the call was refused.
    fn harness_answer(&self) -> String {
        fs::read_to_string(self.workdir.join("out").join("result.txt")).unwrap_or_default()
    }

    /// Everything the run said: the harness's result and the whole trace, for the assertions
    /// that a string appears nowhere at all.
    fn transcript(&self) -> String {
        format!("{}\n{}", self.harness_answer(), self.trace_raw)
    }
}

/// Write the harness wrapper: the fixture script with this scenario's profile and call baked in.
///
/// The profile reaches the harness through its own environment rather than the test process's,
/// which is what lets these cases run in parallel.
fn write_harness(project: &Path, capsule: &Capsule<'_>) -> PathBuf {
    let harness = project.join("harness.sh");
    let mut script = String::from("#!/bin/bash\n");
    script.push_str(&format!(
        "export FIXTURE_HARNESS_PROFILE={}\n",
        capsule.profile
    ));
    script.push_str(&format!("export FIXTURE_BRIDGE_CALLS={}\n", capsule.calls));
    if let Some(tool) = capsule.call_tool {
        script.push_str(&format!("export FIXTURE_BRIDGE_CALL_TOOL='{tool}'\n"));
    }
    if let Some(args) = capsule.call_args {
        script.push_str(&format!("export FIXTURE_BRIDGE_CALL_ARGS='{args}'\n"));
    }
    script.push_str(&format!(
        "exec '{}' \"$@\"\n",
        common::fixture_path("process-driver/fake-harness").display()
    ));
    fs::write(&harness, script).unwrap();
    let mut perms = fs::metadata(&harness).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&harness, perms).unwrap();
    harness
}

fn create_manifest(project: &Path, capsule: &Capsule<'_>, harness: &Path) -> PathBuf {
    let hook_yaml: String = capsule
        .hooks
        .iter()
        .map(|h| {
            format!(
                "  - name: {}\n    version: 0.1.0\n    runtime: hook\n",
                h.name
            )
        })
        .collect();
    let mut tool_yaml = String::new();
    if capsule.marker_tool {
        tool_yaml.push_str(&format!(
            "  - name: {TOOL}\n    version: 0.1.0\n    runtime: tool\n"
        ));
    }
    if capsule.echo_tool {
        tool_yaml.push_str(&format!(
            "  - name: {ECHO_TOOL}\n    version: 0.1.0\n    runtime: tool\n"
        ));
    }
    let shell_yaml = if capsule.shell_allow.is_empty() {
        String::new()
    } else {
        let entries: String = capsule
            .shell_allow
            .iter()
            .map(|b| format!("      - {b}\n"))
            .collect();
        format!("  shell:\n    allow:\n{entries}")
    };
    let read_only_yaml = if capsule.read_only.is_empty() {
        String::new()
    } else {
        let entries: String = capsule
            .read_only
            .iter()
            .map(|e| format!("      - {e}\n"))
            .collect();
        format!("  filesystem:\n    read_only:\n{entries}")
    };
    let limits = capsule
        .deadline_seconds
        .map(|s| format!("  limits:\n    deadline_seconds: {s}\n"))
        .unwrap_or_default();

    let manifest = format!(
        "name: process-gate\n\
         version: 0.1.0\n\
         artifacts:\n\
         \x20 - name: {DRIVER}\n\
         \x20   version: {DRIVER_VERSION}\n\
         \x20   runtime: driver\n\
         {hook_yaml}{tool_yaml}\
         capabilities:\n\
         \x20 env:\n\
         \x20   allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
         {shell_yaml}{read_only_yaml}{limits}\
         inference:\n\
         \x20 transport: process\n\
         \x20 driver:\n\
         \x20   artifact: {DRIVER}\n\
         \x20 command: {harness}\n",
        harness = harness.display(),
    );
    fs::write(project.join("murmur.yaml"), manifest).unwrap();
    project.join("murmur.yaml")
}

/// Publish the driver, the hooks and the tools, stage the capsule, seed its workdir, run the
/// harness and collect what it left.
fn run_session(capsule: &Capsule<'_>) -> Session {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver = common::create_driver_artifact_with_auth(
        artifact_dir.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    common::publish_local(&home, &driver).success();

    for h in &capsule.hooks {
        let artifact = create_hook_zip(
            artifact_dir.path(),
            h.name,
            h.binding,
            h.commit_policy,
            &h.wasm,
        );
        common::publish_local(&home, &artifact).success();
    }
    if capsule.marker_tool {
        let artifact = common::create_native_artifact(
            artifact_dir.path(),
            TOOL,
            "0.1.0",
            &marker_tool_script(),
            Some("Fixture marker tool"),
            None,
        );
        common::publish_local(&home, &artifact).success();
    }
    if capsule.echo_tool {
        let artifact = create_wasm_tool_artifact(artifact_dir.path(), ECHO_TOOL, "0.1.0");
        common::publish_local(&home, &artifact).success();
    }

    let harness = write_harness(project.path(), capsule);
    let manifest_path = create_manifest(project.path(), capsule, &harness);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();

    fs::create_dir_all(workdir.join(PROTECTED_RULE)).unwrap();
    fs::write(workdir.join(PROTECTED_FILE), PROTECTED_CONTENTS).unwrap();
    fs::write(workdir.join("task.md"), "Do the thing.").unwrap();

    launch_session(staged, |_| {}).expect("the session must launch whatever the manifest decides");

    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        trace,
        trace_raw,
        workdir,
        _project: project,
    }
}

// ── S1: a policy hook refuses a bridged call ─────────────────────────────────

/// A `commit_policy: deny` `on-tool-call` hook refuses a call that arrives over the bridge: the
/// tool does not run, the harness is handed the hook's own refusal, and the trace carries the
/// same `call_denied` record the http path writes.
#[test]
fn a_policy_hook_refuses_a_bridged_tool_call() {
    if common::skip_without_host_support("a_policy_hook_refuses_a_bridged_tool_call") {
        return;
    }
    let session = run_session(&Capsule::calling_the_marker_tool().with_hook(hook(
        "tool-policy",
        "on-tool-call",
        "deny",
        deny_hook_wasm("on-tool-call", "no tool calls today"),
    )));

    assert!(
        !session.marker_exists(),
        "the tool never ran, so it wrote nothing:\n{}",
        session.trace_raw
    );
    let transcript = session.transcript();
    assert!(
        !transcript.contains(TOOL_OUTPUT),
        "the tool's own output appears nowhere in the run:\n{transcript}"
    );

    let answer = session.harness_answer();
    assert!(answer.contains("tool-policy"), "names the hook: {answer}");
    assert!(
        answer.contains("no tool calls today"),
        "carries the hook's reason: {answer}"
    );
    assert!(
        answer.contains("did not run"),
        "the no-retry sentence reaches the harness intact: {answer}"
    );

    let denial = session.denial();
    assert_eq!(denial["event"], "on-tool-call");
    assert_eq!(denial["hook_name"], "tool-policy");
    assert_eq!(denial["target"], TOOL, "the bare tool name");
    assert_eq!(denial["reason"], "no tool calls today");
}

// ── S3: a read_only write is refused before the tool runs ────────────────────

/// A `bash` command that writes a declared read-only path is refused before anything is spawned,
/// with no policy hook anywhere in the capsule.
#[test]
fn a_read_only_write_is_refused_on_the_bridge() {
    if common::skip_without_host_support("a_read_only_write_is_refused_on_the_bridge") {
        return;
    }
    let args = format!(r#"{{"command":"echo broken > {PROTECTED_FILE}"}}"#);
    let session = run_session(&Capsule {
        marker_tool: false,
        read_only: &[PROTECTED_RULE],
        shell_allow: &["bash"],
        call_tool: Some("bash"),
        call_args: Some(&args),
        ..Capsule::calling_the_marker_tool()
    });

    assert_eq!(
        fs::read_to_string(session.workdir.join(PROTECTED_FILE)).unwrap(),
        PROTECTED_CONTENTS,
        "no subprocess ran, so the protected file is byte-identical"
    );
    assert!(
        session.events("shell").is_empty(),
        "nothing was dispatched:\n{}",
        session.trace_raw
    );

    let answer = session.harness_answer();
    assert!(answer.contains(PROTECTED_FILE), "names the path: {answer}");
    assert!(
        answer.contains(&format!("'{PROTECTED_RULE}'")),
        "names the rule: {answer}"
    );
    assert!(answer.contains("Nothing ran"), "{answer}");
    assert!(
        answer.contains("still readable"),
        "the harness is told what is still true: {answer}"
    );

    let refusal = session.refusal();
    assert_eq!(refusal["call"], "shell");
    assert_eq!(refusal["path"], PROTECTED_FILE);
    assert_eq!(refusal["rule"], PROTECTED_RULE);
    let signal = refusal["signal"].as_str().unwrap();
    assert!(
        signal.contains("redirection") && signal.contains('>'),
        "the signal names the redirection: {signal}"
    );
    assert!(
        refusal["reason"]
            .as_str()
            .unwrap()
            .contains("capabilities.filesystem.read_only"),
        "{refusal:#?}"
    );
}

// ── S4: an allowing hook changes nothing, and what the gate costs ────────────

/// An `on-tool-call` hook that returns `none` leaves the call exactly as it was: the tool runs,
/// the harness gets its output, and nothing is recorded as refused.
#[test]
fn an_allowing_hook_leaves_a_bridged_call_alone() {
    if common::skip_without_host_support("an_allowing_hook_leaves_a_bridged_call_alone") {
        return;
    }
    let session = run_session(&Capsule::calling_the_marker_tool().with_hook(hook(
        "quiet-policy",
        "on-tool-call",
        "deny",
        none_hook_wasm("on-tool-call"),
    )));

    assert!(
        session.marker_exists(),
        "the tool ran:\n{}",
        session.trace_raw
    );
    let answer = session.harness_answer();
    assert!(answer.contains(TOOL_OUTPUT), "{answer}");
    assert!(
        answer.contains("\"isError\":false"),
        "the bridge answered with a success: {answer}"
    );
    assert!(
        session.events("call_denied").is_empty(),
        "nothing was refused:\n{}",
        session.trace_raw
    );
}

/// What the decision point costs a bridged call, as a per-call figure a build summary can name.
///
/// The same harness, the same number of bridged calls, run once with a `commit_policy: deny`
/// hook armed and once with nothing that can refuse a call at all. Nothing is asserted about the
/// number: it is a measurement, and a machine under load would make an assertion on it a flake.
#[test]
fn the_gated_path_reports_its_per_call_cost() {
    if common::skip_without_host_support("the_gated_path_reports_its_per_call_cost") {
        return;
    }
    const CALLS: u32 = 100;
    const ROUNDS: usize = 5;

    let bench = || Capsule {
        marker_tool: false,
        echo_tool: true,
        profile: "bridge-bench",
        call_tool: None,
        call_args: None,
        calls: CALLS,
        ..Capsule::calling_the_marker_tool()
    };

    // Spawn to reap, from the run's own `harness_exit` record, so staging, publishing and session
    // setup are outside the window.
    let harness_ms = |capsule: &Capsule<'_>| -> i64 {
        let session = run_session(capsule);
        session.events("harness_exit")[0]["duration_ms"]
            .as_i64()
            .expect("every harness_exit carries its duration")
    };

    let ungated = bench();
    let gated = bench().with_hook(hook(
        "quiet-policy",
        "on-tool-call",
        "deny",
        none_hook_wasm("on-tool-call"),
    ));

    // Paired and alternating, then the median: a machine's clock speed can drift by more over a
    // minute than the gate costs over a hundred calls, and running the two sides back to back
    // is what keeps that drift out of the difference.
    let mut deltas: Vec<i64> = (0..ROUNDS)
        .map(|_| harness_ms(&gated) - harness_ms(&ungated))
        .collect();
    deltas.sort_unstable();
    let per_call = (deltas[ROUNDS / 2] * 1_000) as f64 / f64::from(CALLS);
    println!(
        "gate cost over {CALLS} bridged calls, median of {ROUNDS} paired runs: \
         deltas {deltas:?} ms, {per_call:.0} us per call"
    );
}

// ── S5: a hanging hook does not wedge the event loop ─────────────────────────

/// A hook that never returns is cut off by its own epoch deadline and comes back as a denial, so
/// the run ends on the harness's own terminal event rather than on the inactivity window.
#[test]
fn a_hook_that_never_returns_refuses_and_the_run_ends() {
    if common::skip_without_host_support("a_hook_that_never_returns_refuses_and_the_run_ends") {
        return;
    }
    let started = Instant::now();
    let session = run_session(
        &Capsule::calling_the_marker_tool()
            .with_hook(hook(
                "spinner",
                "on-tool-call",
                "deny",
                spin_hook_wasm("on-tool-call"),
            ))
            .deadline_seconds(1),
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(120),
        "the run ended on the hook's deadline, well inside the inactivity window: {elapsed:?}"
    );
    assert!(
        !session.marker_exists(),
        "the tool never ran:\n{}",
        session.trace_raw
    );

    let answer = session.harness_answer();
    assert!(answer.contains("spinner"), "names the hook: {answer}");
    assert!(answer.contains("deadline"), "names the deadline: {answer}");
    assert_eq!(session.denial()["hook_name"], "spinner");

    let exits = session.events("harness_exit");
    assert_eq!(exits.len(), 1, "{}", session.trace_raw);
    assert_eq!(
        exits[0]["cause"], "terminal",
        "the harness ended on its own terminal event:\n{}",
        session.trace_raw
    );
}
