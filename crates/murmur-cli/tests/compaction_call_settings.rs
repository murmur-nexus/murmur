//! The compaction call set apart from the agent's turns: `inference.compaction.max_tokens` caps every
//! `run-inference` call a hook makes while it serves `on-compaction`, and
//! `inference.compaction.driver_config` is laid over `inference.driver.config` for those calls
//! only. A hook's own `capabilities.limits.deadline_seconds` bounds its calls.
//!
//! Driven through the real `mur` binary and the openai driver fixture against a scripted provider,
//! so every assertion about a cap or an effort is about a request body that crossed a socket.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::PathBuf, time::Duration};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-openai";
const DRIVER_VERSION: &str = "0.1.0";
const COMPACTOR: &str = "local/compactor";
/// Served on the Responses surface, where the cap travels as `max_output_tokens` and the effort
/// as `reasoning.effort`. Listed in [`common::FIXTURE_DRIVER_MODELS`].
const MODEL: &str = "gpt-5";
const BEARER_AUTH: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
const TASK: &str = "Call the tool, then finish.";
/// `inference.max_tokens` here: every agent turn's cap.
const TURN_CAP: u64 = 4096;
/// `context.max_tokens` and the threshold: compaction fires at 50 000 tokens, which only the first
/// turn's reported usage reaches.
const CONTEXT: &str = "context:\n  max_tokens: 100000\n";
const SUMMARY: &str = "the conversation so far, in one line";

// ── Scripted provider responses ───────────────────────────────────────────────

/// A Responses reply asking for a tool the capsule does not have, reporting enough input to cross
/// the compaction threshold, so the loop compacts before it takes another turn.
fn tool_call() -> String {
    json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "no-such-tool",
            "arguments": "{}"
        }],
        "usage": {"input_tokens": 60000, "output_tokens": 5}
    })
    .to_string()
}

/// A Responses reply saying `text`.
fn text(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// The first turn, the compaction call's summary, and the turn that ends the task.
fn compacting_script() -> Vec<String> {
    vec![
        tool_call(),
        text("resp_2", SUMMARY),
        text("resp_3", "Task complete."),
    ]
}

// ── Homes, projects and runs ──────────────────────────────────────────────────

/// A scratch `HOME` with the openai driver and the `compactor` hook, built from `hook_wasm`,
/// published into it.
fn home(hook_wasm: &[u8]) -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/openai/driver/murmur-driver-openai.wasm"),
        &format!("{BEARER_AUTH}{}", common::FIXTURE_DRIVER_MODELS),
    );
    common::publish_local(&home, &driver).success();
    let hook = common::hook_wat::create_hook_zip(
        artifacts.path(),
        COMPACTOR,
        "on-compaction",
        "replace-context",
        hook_wasm,
    );
    common::publish_local(&home, &hook).success();
    home
}

/// A home whose compactor summarizes through `run-inference`.
fn summarizing_home() -> TempDir {
    home(&common::hook_wat::run_inference_then_replace_compaction_hook_wasm())
}

/// A capsule on [`MODEL`] with `driver.config.reasoning_effort: high`, reaching `endpoint`, the
/// compactor declared with `hook_extra` spliced into its entry and `compaction_extra` into
/// `inference.compaction`.
fn manifest(endpoint: &str, hook_extra: &str, compaction_extra: &str) -> String {
    format!(
        "name: compaction-call\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\n  - name: {COMPACTOR}\n    version: 0.1.0\n    runtime: hook\n\
         {hook_extra}capabilities:\n  network:\n    allow:\n      - {endpoint}\n{CONTEXT}\
         inference:\n  transport: http\n  model: {MODEL}\n  max_tokens: {TURN_CAP}\n  driver:\n    \
         artifact: {DRIVER}\n    config:\n      reasoning_effort: high\n  compaction:\n    \
         threshold: 0.5\n{compaction_extra}"
    )
}

/// Both compaction keys: cap 2000, effort low.
const BOTH_KEYS: &str = "    max_tokens: 2000\n    driver_config:\n      reasoning_effort: low\n";

struct Project {
    dir: TempDir,
    manifest: PathBuf,
}

fn project(manifest_yaml: &str) -> Project {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(&manifest, manifest_yaml).unwrap();
    Project { dir, manifest }
}

/// What one `mur run` or `mur doctor` left behind.
struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(
            &common::parse_records_from_stdout(&self.stdout).join("trace.jsonl"),
        )
    }

    /// Every stderr line naming `code`.
    fn lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(code))
            .collect()
    }
}

fn run(home: &TempDir, project: &Project) -> Run {
    Run::of(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .current_dir(project.dir.path())
            .args([
                "run",
                "--manifest",
                project.manifest.to_str().unwrap(),
                "--task",
                TASK,
                "--verbose",
            ])
            .output()
            .unwrap(),
    )
}

fn doctor(home: &TempDir, project: &Project) -> Run {
    Run::of(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .current_dir(project.dir.path())
            .arg("doctor")
            .output()
            .unwrap(),
    )
}

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// A request's output cap and reasoning effort, as the driver put them on the wire.
fn cap_and_effort(request: &Value) -> (Value, Value) {
    (
        request["max_output_tokens"].clone(),
        request["reasoning"]["effort"].clone(),
    )
}

/// Whether `request` is the compaction call: the agent's turns carry tools, a hook's
/// `run-inference` none.
fn is_compaction_call(request: &Value) -> bool {
    request
        .get("tools")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

/// Runs [`compacting_script`] under `compaction_extra`, asserts it completed with one compaction,
/// and returns the three requests the provider received, in order.
fn run_compacting(compaction_extra: &str) -> Vec<Value> {
    let home = summarizing_home();
    let server = common::ScriptedServer::start(compacting_script());
    let project = project(&manifest(&server.endpoint, "", compaction_extra));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let trace = run.trace();
    assert_eq!(
        events(&trace, "compaction").len(),
        1,
        "{}\n{trace:#?}",
        run.context()
    );
    let requests = server.requests();
    for request in &requests {
        println!("request: {:?}", cap_and_effort(request));
    }
    assert_eq!(requests.len(), 3, "{}", run.context());
    assert_eq!(
        requests.iter().map(is_compaction_call).collect::<Vec<_>>(),
        [false, true, false],
        "turn, compaction call, turn: {requests:#?}"
    );
    requests
}

// ── S4, S7: what the compaction call sends ────────────────────────────────────

#[test]
fn the_compaction_call_sends_its_own_cap_and_effort() {
    let requests = run_compacting(BOTH_KEYS);
    assert_eq!(cap_and_effort(&requests[1]), (json!(2000), json!("low")));
    for turn in [&requests[0], &requests[2]] {
        assert_eq!(cap_and_effort(turn), (json!(TURN_CAP), json!("high")));
    }
}

#[test]
fn unset_the_compaction_call_sends_the_ceiling_and_the_agents_effort() {
    let requests = run_compacting("");
    assert_eq!(
        cap_and_effort(&requests[1]),
        (json!(common::FIXTURE_MAX_OUTPUT_TOKENS), json!("high"))
    );
    for turn in [&requests[0], &requests[2]] {
        assert_eq!(cap_and_effort(turn), (json!(TURN_CAP), json!("high")));
    }
}

// ── S5: a cap above the ceiling ───────────────────────────────────────────────

#[test]
fn a_compaction_cap_above_the_ceiling_is_refused_at_launch_and_by_doctor() {
    let home = summarizing_home();
    let server = common::ScriptedServer::start(compacting_script());
    let project = project(&manifest(&server.endpoint, "", "    max_tokens: 9000\n"));
    let expected = format!(
        "inference.compaction.max_tokens (9000) is above the output ceiling of 8192 that driver \
         {DRIVER}@{DRIVER_VERSION} declares for compaction model '{MODEL}'"
    );

    let run = run(&home, &project);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let refused = run.lines("error[E-RUN-054]");
    println!("{refused:?}");
    assert_eq!(refused.len(), 1, "{}", run.context());
    assert!(refused[0].contains(&expected), "{}", run.context());
    assert!(
        run.stderr
            .contains("lower inference.compaction.max_tokens to at most 8192"),
        "{}",
        run.context()
    );
    assert!(server.requests().is_empty(), "nothing reached the provider");

    let doctor = doctor(&home, &project);
    println!("{}", doctor.stdout);
    assert_eq!(doctor.output.status.code(), Some(1), "{}", doctor.context());
    assert!(
        doctor
            .stdout
            .contains(&format!("error[E-RUN-054]: {expected}")),
        "{}",
        doctor.context()
    );
    assert!(
        doctor.stdout.contains(
            "lower inference.compaction.max_tokens to at most 8192, or remove it to use the \
             driver's ceiling"
        ),
        "{}",
        doctor.context()
    );
}

// ── S6: the driver refuses a key of the merged config ─────────────────────────

#[test]
fn the_driver_refuses_an_unknown_overlay_key_on_the_compaction_call() {
    let home = summarizing_home();
    let server = common::ScriptedServer::start(compacting_script());
    let project = project(&manifest(
        &server.endpoint,
        "",
        "    driver_config:\n      bogus_key: 1\n",
    ));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "the first turn ran and the compaction call never left: {}",
        run.context()
    );
    assert!(!is_compaction_call(&requests[0]));
    assert_eq!(
        cap_and_effort(&requests[0]),
        (json!(TURN_CAP), json!("high"))
    );

    let failed = run.lines("error[E-RUN-040]");
    println!("{failed:?}");
    assert_eq!(failed.len(), 1, "{}", run.context());
    assert!(
        failed[0].contains("compaction hook failed"),
        "{}",
        failed[0]
    );
    assert!(
        failed[0].contains("unrecognised inference.driver.config key(s): bogus_key"),
        "{}",
        failed[0]
    );
}

// ── S8: a hook's own deadline ─────────────────────────────────────────────────

/// The bare name hook messages call [`COMPACTOR`] by.
const COMPACTOR_TOOL: &str = "compactor";

/// A blocking `on-compaction` hook that never returns, its entry setting `deadline_seconds: 2` and
/// `extra_limits`. The capsule sets no capsule-wide limits, so only the entry can end it in 2 s.
/// The scratch `HOME`, which holds the session's records, is returned beside the run.
fn run_spinning_compactor(extra_limits: &str) -> (Run, TempDir) {
    let home = home(&common::hook_wat::spin_hook_wasm("on-compaction"));
    let server = common::ScriptedServer::start(compacting_script());
    let project = project(&manifest(
        &server.endpoint,
        &format!("    capabilities:\n      limits:\n        deadline_seconds: 2\n{extra_limits}"),
        "",
    ));
    (run(&home, &project), home)
}

/// The time from the first agent turn's `inference` line to `session_end`: the compaction call and
/// teardown, without the launch's own staging, which compiles components and is slow under load.
fn turn_to_end(trace: &[Value]) -> Duration {
    let at = |event: &Value| event["timestamp"].as_u64().expect("timestamp");
    let first_turn = events(trace, "inference")
        .first()
        .map(|event| at(event))
        .expect("the first turn ran");
    let end = events(trace, "session_end")
        .first()
        .map(|event| at(event))
        .expect("session_end is written");
    Duration::from_millis(end - first_turn)
}

#[test]
fn a_compaction_hook_stops_at_its_entrys_deadline() {
    let (run, _home) = run_spinning_compactor("");
    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());
    let elapsed = turn_to_end(&run.trace());
    println!("first turn to session_end: {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(25),
        "the entry's 2s fired, not the 30s default: {elapsed:?}"
    );
    let failed = run.lines("error[E-RUN-040]");
    println!("{failed:?}");
    assert_eq!(failed.len(), 1, "{}", run.context());
    assert!(
        failed[0].contains(&format!(
            "compaction hook failed: {COMPACTOR_TOOL}: hook '{COMPACTOR_TOOL}' on-compaction \
             exceeded its 2s \
             execution deadline (capabilities.limits.deadline_seconds on the artifacts: entry \
             '{COMPACTOR}') and was interrupted"
        )),
        "{}",
        failed[0]
    );
    assert!(run.lines("W-SEC-006").is_empty(), "{}", run.context());
}

#[test]
fn a_hook_entrys_other_limits_draw_w_sec_006_by_their_full_key() {
    let (run, _home) = run_spinning_compactor("        memory_bytes: 1073741824\n");
    let warned = run.lines("warning[W-SEC-006]");
    println!("{warned:?}");
    assert_eq!(warned.len(), 1, "{}", run.context());
    assert!(
        warned[0].contains(&format!(
            "hook '{COMPACTOR_TOOL}' declares capabilities.limits.memory_bytes which"
        )),
        "{}",
        warned[0]
    );
    assert!(
        !warned[0].contains("declares capabilities.limits which"),
        "{}",
        warned[0]
    );
}
