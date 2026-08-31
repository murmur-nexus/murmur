//! End-to-end coverage for the `deny` commit arm: what a policy hook stops, what the agent is
//! told, and what `trace.jsonl` records about a call that never ran.
//!
//! The hook components are hand-authored WAT compiled in-test, so nothing here depends on a
//! `default-artifacts` checkout and no case is `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use common::hook_wat::{
    artifact_hook_wasm, create_hook_zip, deny_hook_wasm, none_hook_wasm, shell_echo_deny_hook_wasm,
    spin_hook_wasm, trap_hook_wasm, unreadable_output_hook_wasm,
};
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The file a denied `bash` call would have created. Its absence is the measurement.
const MARKER: &str = "marker.txt";

/// The command the model asks for in every shell scenario: it writes [`MARKER`] into the
/// workdir using nothing but `bash` builtins, so a kernel-enforcement host that grants only
/// `bash` still runs it.
fn marker_command() -> String {
    format!(": > {MARKER}")
}

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn tool_call(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
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

/// A capsule manifest declaring the driver, the given hooks and tools, and `bash`.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    hooks: &[Hook<'_>],
    tools: &[(&str, &str)],
    deadline_seconds: Option<u64>,
) -> PathBuf {
    let hook_yaml: String = hooks
        .iter()
        .map(|h| {
            format!(
                "  - name: {}\n    version: 0.1.0\n    runtime: hook\n",
                h.name
            )
        })
        .collect();
    let tool_yaml: String = tools
        .iter()
        .map(|(name, version)| {
            format!("  - name: {name}\n    version: {version}\n    runtime: tool\n")
        })
        .collect();
    let limits = deadline_seconds
        .map(|s| format!("  limits:\n    deadline_seconds: {s}\n"))
        .unwrap_or_default();

    let manifest = format!(
        concat!(
            "name: deny-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "{hook_yaml}",
            "{tool_yaml}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "{limits}",
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
        hook_yaml = hook_yaml,
        tool_yaml = tool_yaml,
        endpoint = endpoint,
        limits = limits,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// What one launched session left behind.
struct Session {
    requests: Vec<Value>,
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

    /// The one `call_denied` line, with the whole trace in the failure message — a refusal
    /// that did not get recorded is the defect these tests exist to catch.
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

    fn marker_exists(&self) -> bool {
        self.workdir.join(MARKER).exists()
    }

    /// The text of the tool result the driver was handed for `tool_use_id`.
    fn tool_result_text(&self, tool_use_id: &str) -> String {
        let block = common::find_tool_result(&self.requests, tool_use_id)
            .unwrap_or_else(|| panic!("no tool_result for {tool_use_id}"));
        common::extract_result_text(&block)
    }
}

/// Publish the driver, every hook and every native tool, stage the capsule, run it, and
/// collect what it left.
fn run_session(
    responses: Vec<String>,
    hooks: &[Hook<'_>],
    native_tools: &[(&str, &str)],
    deadline_seconds: Option<u64>,
) -> Session {
    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    for h in hooks {
        let artifact = create_hook_zip(
            artifact_dir.path(),
            h.name,
            h.binding,
            h.commit_policy,
            &h.wasm,
        );
        common::publish_local(&home, &artifact).success();
    }

    let mut tool_refs = Vec::new();
    for (name, script) in native_tools {
        let artifact = common::create_native_artifact(
            artifact_dir.path(),
            name,
            "0.1.0",
            script,
            Some("Fixture tool"),
            None,
        );
        common::publish_local(&home, &artifact).success();
        tool_refs.push((*name, "0.1.0"));
    }

    let manifest_path = create_manifest(
        project.path(),
        &server.endpoint,
        hooks,
        &tool_refs,
        deadline_seconds,
    );
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Do the thing.").unwrap();

    launch_session(staged, |_| {}).expect("the session must launch whatever the policy decides");

    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        requests: server.requests(),
        trace,
        trace_raw,
        workdir,
        _project: project,
    }
}

/// One `bash` call the hook is asked about, then an `end_turn`.
fn one_shell_call(command: &str) -> Vec<String> {
    vec![
        tool_call("msg_1", "toolu_bash", "bash", json!({"command": command})),
        end_turn("msg_2", "Understood."),
    ]
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// A denying `on-shell` hook stops the command: nothing runs, the agent is told which hook
/// refused and why, and the trace carries a `call_denied` and no `shell` record.
#[test]
fn denying_on_shell_hook_stops_the_command() {
    if common::skip_without_host_support("denying_on_shell_hook_stops_the_command") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "branch-policy",
            "on-shell",
            "deny",
            deny_hook_wasm("on-shell", "protected branch"),
        )],
        &[],
        None,
    );

    assert!(!session.marker_exists(), "the denied command must not run");
    // The runtime marks the message `is_error: true`; the Anthropic driver fixture does not
    // carry that flag onto the wire block, so what is asserted here is what the model
    // actually reads. `denial_tool_result_names_the_hook_and_refuses_a_retry` in
    // `capsule-runtime` pins the text itself.
    let text = session.tool_result_text("toolu_bash");
    assert!(
        text.contains("branch-policy"),
        "reason names the hook: {text}"
    );
    assert!(
        text.contains("protected branch"),
        "reason is the hook's: {text}"
    );
    assert!(
        text.contains("Retrying it unchanged will be refused again"),
        "the agent is told not to retry: {text}"
    );

    let denial = session.denial();
    assert_eq!(denial["event"], "on-shell");
    assert_eq!(denial["hook_name"], "branch-policy");
    assert_eq!(denial["reason"], "protected branch");
    let target = denial["target"].as_str().unwrap();
    assert!(
        target.ends_with("bash"),
        "target is the resolved bash path: {target}"
    );

    assert!(
        session.events("shell").is_empty(),
        "nothing ran, so nothing is recorded as having run:\n{}",
        session.trace_raw
    );
}

/// A denying `on-tool-call` hook stops the tool: its side effect is absent and the trace
/// names the tool rather than a binary.
#[test]
fn denying_on_tool_call_hook_stops_the_tool() {
    if common::skip_without_host_support("denying_on_tool_call_hook_stops_the_tool") {
        return;
    }
    let script = format!(
        "#!/bin/sh\n: > {MARKER}\necho '{}'\n",
        r#"{"status":"passed","summary":"ok","data":null,"data_path":null,"truncated":false,"metadata":[]}"#
    );
    let session = run_session(
        vec![
            tool_call("msg_1", "toolu_spend", "spender", json!({"amount": 10})),
            end_turn("msg_2", "Understood."),
        ],
        &[hook(
            "spend-policy",
            "on-tool-call",
            "deny",
            deny_hook_wasm("on-tool-call", "spend-sensitive"),
        )],
        &[("spender", script.as_str())],
        None,
    );

    assert!(!session.marker_exists(), "the denied tool must not run");
    let text = session.tool_result_text("toolu_spend");
    assert!(text.contains("spend-policy"), "{text}");
    assert!(text.contains("spend-sensitive"), "{text}");

    let denial = session.denial();
    assert_eq!(denial["event"], "on-tool-call");
    assert_eq!(denial["target"], "spender");
    assert_eq!(denial["reason"], "spend-sensitive");

    assert!(
        !session
            .events("tool_call")
            .iter()
            .any(|e| e["tool_name"] == "spender"),
        "a denied tool call is not recorded as a tool call:\n{}",
        session.trace_raw
    );
}

/// A hook that traps denies, and the session still ends normally.
#[test]
fn a_hook_that_panics_denies() {
    if common::skip_without_host_support("a_hook_that_panics_denies") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "trapper",
            "on-shell",
            "deny",
            trap_hook_wasm("on-shell"),
        )],
        &[],
        None,
    );

    assert!(!session.marker_exists());
    let text = session.tool_result_text("toolu_bash");
    assert!(text.contains("trapper"), "{text}");
    assert!(
        text.contains("trap") || text.contains("unreachable"),
        "the reason names the trap: {text}"
    );
    assert_eq!(session.denial()["hook_name"], "trapper");
    assert_eq!(
        session.events("session_end").len(),
        1,
        "a denial is not a session failure"
    );
}

/// A hook whose return the host cannot lift denies.
#[test]
fn a_hook_with_unreadable_output_denies() {
    if common::skip_without_host_support("a_hook_with_unreadable_output_denies") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "stale-abi",
            "on-shell",
            "deny",
            unreadable_output_hook_wasm("on-shell"),
        )],
        &[],
        None,
    );

    assert!(!session.marker_exists());
    let text = session.tool_result_text("toolu_bash");
    assert!(text.contains("stale-abi"), "{text}");
    assert_eq!(session.denial()["hook_name"], "stale-abi");
}

/// A hook returning `deny("")` denies, with a runtime-supplied reason naming the defect.
#[test]
fn a_hook_denying_with_an_empty_reason_denies() {
    if common::skip_without_host_support("a_hook_denying_with_an_empty_reason_denies") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "mute-policy",
            "on-shell",
            "deny",
            deny_hook_wasm("on-shell", ""),
        )],
        &[],
        None,
    );

    assert!(!session.marker_exists());
    let text = session.tool_result_text("toolu_bash");
    assert!(text.contains("mute-policy"), "{text}");
    assert!(text.contains("empty reason"), "{text}");
    let denial = session.denial();
    assert_eq!(denial["hook_name"], "mute-policy");
    assert!(denial["reason"].as_str().unwrap().contains("empty reason"));
}

/// A hook that never returns denies when its epoch deadline expires, and the test finishes.
#[test]
fn a_hook_that_times_out_denies() {
    if common::skip_without_host_support("a_hook_that_times_out_denies") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "spinner",
            "on-shell",
            "deny",
            spin_hook_wasm("on-shell"),
        )],
        &[],
        Some(1),
    );

    assert!(!session.marker_exists());
    let text = session.tool_result_text("toolu_bash");
    assert!(text.contains("spinner"), "{text}");
    assert!(
        text.contains("deadline"),
        "the reason names the deadline: {text}"
    );
    assert_eq!(session.denial()["hook_name"], "spinner");
}

/// A non-`deny` arm at the decision point denies **and** is a dispatch fault.
#[test]
fn a_non_deny_arm_at_the_decision_point_denies_and_is_a_fault() {
    if common::skip_without_host_support(
        "a_non_deny_arm_at_the_decision_point_denies_and_is_a_fault",
    ) {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "wrong-arm",
            "on-shell",
            "deny",
            artifact_hook_wasm("on-shell", "{}"),
        )],
        &[],
        None,
    );

    assert!(!session.marker_exists());
    let text = session.tool_result_text("toolu_bash");
    assert!(
        text.contains("artifact"),
        "the reason names the arm: {text}"
    );
    assert_eq!(session.denial()["hook_name"], "wrong-arm");

    let faults = session.events("hook_dispatch_error");
    assert!(
        faults
            .iter()
            .any(|e| e["hook_name"] == "wrong-arm" && e["arm"] == "artifact"),
        "the unsupported arm is also traced as a fault:\n{}",
        session.trace_raw
    );
}

/// `deny` returned from an event with no decision point is a fault, not a denial: nothing is
/// refused and the session completes.
#[test]
fn deny_from_on_inference_is_a_fault_not_a_denial() {
    if common::skip_without_host_support("deny_from_on_inference_is_a_fault_not_a_denial") {
        return;
    }
    let session = run_session(
        one_shell_call("echo hello"),
        &[hook(
            "watcher",
            "on-inference",
            "none",
            deny_hook_wasm("on-inference", "no"),
        )],
        &[],
        None,
    );

    assert!(
        session.events("call_denied").is_empty(),
        "an ungated event refuses nothing:\n{}",
        session.trace_raw
    );
    assert!(
        session
            .events("hook_dispatch_error")
            .iter()
            .any(|e| e["hook_name"] == "watcher"
                && e["event"] == "on-inference"
                && e["arm"] == "deny"),
        "the arm is reported as a fault:\n{}",
        session.trace_raw
    );
}

/// The same for an *observer* bound to a gated event: `commit_policy: none` means no decision
/// point, so its `deny` lands on the observation dispatch and is discarded.
#[test]
fn deny_from_an_on_shell_observer_is_a_fault_not_a_denial() {
    if common::skip_without_host_support("deny_from_an_on_shell_observer_is_a_fault_not_a_denial") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "observer",
            "on-shell",
            "none",
            deny_hook_wasm("on-shell", "no"),
        )],
        &[],
        None,
    );

    assert!(
        session.marker_exists(),
        "an observer refuses nothing; the command ran"
    );
    assert!(
        session.events("call_denied").is_empty(),
        "trace was:\n{}",
        session.trace_raw
    );
    assert!(
        session
            .events("hook_dispatch_error")
            .iter()
            .any(|e| e["hook_name"] == "observer"
                && e["event"] == "on-shell"
                && e["arm"] == "deny"),
        "trace was:\n{}",
        session.trace_raw
    );
}

/// A policy hook cannot permit what the manifest does not allow: an undeclared binary is still
/// refused with today's message, and the hook never sees a shell decision for it.
#[test]
fn a_policy_hook_cannot_permit_an_undeclared_binary() {
    if common::skip_without_host_support("a_policy_hook_cannot_permit_an_undeclared_binary") {
        return;
    }
    let session = run_session(
        vec![
            tool_call(
                "msg_1",
                "toolu_curl",
                "curl",
                json!({"command": "curl --version"}),
            ),
            end_turn("msg_2", "Refused."),
        ],
        &[hook(
            "permissive",
            "on-shell",
            "deny",
            none_hook_wasm("on-shell"),
        )],
        &[],
        None,
    );

    let text = session.tool_result_text("toolu_curl");
    assert!(text.contains("curl"), "{text}");
    assert!(
        text.contains("not declared in manifest allowlist"),
        "{text}"
    );
    assert!(
        session.events("call_denied").is_empty(),
        "the manifest refused it, not a hook:\n{}",
        session.trace_raw
    );
}

/// The hook decides on what will actually run: the untruncated `-c` script body and the
/// resolved absolute path of the interpreter.
#[test]
fn the_hook_is_shown_what_will_actually_run() {
    if common::skip_without_host_support("the_hook_is_shown_what_will_actually_run") {
        return;
    }
    // Well past `command`'s 200-character clip, so a policy reading `command` would see a
    // different string from the one that would have executed.
    let script = format!("echo {}", "abcdefghij".repeat(30));
    assert!(script.len() > 200);

    let session = run_session(
        one_shell_call(&script),
        &[hook(
            "echoer",
            "on-shell",
            "deny",
            shell_echo_deny_hook_wasm(),
        )],
        &[],
        None,
    );

    let reason = session.denial()["reason"].as_str().unwrap().to_string();
    let mut parts = reason.splitn(3, '|');
    let binary = parts.next().unwrap();
    let seen_script = parts.next().unwrap();
    let argv = parts.next().unwrap();

    assert!(
        binary.starts_with('/') && binary.ends_with("bash"),
        "binary is a resolved absolute path: {binary}"
    );
    assert_eq!(seen_script, script, "the script body arrives untruncated");
    assert_eq!(argv.trim_end(), format!("-c {script}"), "argv is exact");
}

/// A capsule with only an observer hook does no gating: the shell call runs, nothing is
/// denied, and the existing shell assertions hold unchanged.
#[test]
fn an_observer_only_capsule_gates_nothing() {
    if common::skip_without_host_support("an_observer_only_capsule_gates_nothing") {
        return;
    }
    let session = run_session(
        one_shell_call(&marker_command()),
        &[hook(
            "observer",
            "on-shell",
            "none",
            none_hook_wasm("on-shell"),
        )],
        &[],
        None,
    );

    assert!(session.marker_exists(), "the command ran");
    assert!(
        session.events("call_denied").is_empty(),
        "trace was:\n{}",
        session.trace_raw
    );
    assert_eq!(
        session.events("shell").len(),
        1,
        "the shell call is recorded as usual:\n{}",
        session.trace_raw
    );
}

// ── Staging refusals ─────────────────────────────────────────────────────────

/// Three misconfigured policy hooks, each refused at staging — before a session exists, so
/// before anything the hook was meant to gate could run.
#[test]
fn a_misconfigured_policy_hook_is_refused_at_launch() {
    if common::skip_without_host_support("a_misconfigured_policy_hook_is_refused_at_launch") {
        return;
    }
    for (binding, commit_policy, mode, expected) in [
        (
            "on-compaction",
            "deny",
            "blocking",
            "is not valid for binding",
        ),
        ("", "deny", "blocking", "requires an explicit binding"),
        (
            "on-shell",
            "deny",
            "async",
            "async-with-commit not supported",
        ),
    ] {
        let err = stage_failure(binding, commit_policy, mode);
        assert!(
            err.contains(expected),
            "expected {expected:?} in the staging error, got: {err}"
        );
        assert!(
            err.contains("bad-policy"),
            "the error names the hook, got: {err}"
        );
    }
}

/// Stage a capsule carrying one hook with the given contract via `mur run`, and return what it
/// wrote to stderr. Staging happens before the capsule is launched, so a refusal here is a
/// refusal before any session starts.
fn stage_failure(binding: &str, commit_policy: &str, execution_mode: &str) -> String {
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    // Written here rather than through `create_hook_zip` because these manifests must be able
    // to omit `binding:` entirely and to carry an `execution_mode:`.
    let artifact_path = artifact_dir.join_hook("bad-policy");
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: bad-policy").unwrap();
    writeln!(zip, "version: 0.1.0").unwrap();
    writeln!(zip, "runtime: hook").unwrap();
    if !binding.is_empty() {
        writeln!(zip, "binding: {binding}").unwrap();
    }
    writeln!(zip, "commit_policy: {commit_policy}").unwrap();
    writeln!(zip, "execution_mode: {execution_mode}").unwrap();
    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&none_hook_wasm("on-shell")).unwrap();
    zip.finish().unwrap();

    common::publish_local(&home, &artifact_path).success();

    let manifest_path = create_manifest(
        project.path(),
        "http://127.0.0.1:9",
        &[hook("bad-policy", binding, commit_policy, Vec::new())],
        &[],
        None,
    );
    fs::write(project.path().join("task.md"), "Do the thing.").unwrap();
    let assert = common::run_capsule(&home, &manifest_path).failure();
    String::from_utf8_lossy(&assert.get_output().stderr).to_string()
}

/// Name the hook artifact zip the way `create_hook_zip` does, so `publish_local` accepts it.
trait HookZipPath {
    fn join_hook(&self, name: &str) -> PathBuf;
}

impl HookZipPath for tempfile::TempDir {
    fn join_hook(&self, name: &str) -> PathBuf {
        self.path().join(format!("{name}-0.1.0.mur.zip"))
    }
}
