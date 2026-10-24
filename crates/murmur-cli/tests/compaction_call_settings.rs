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
/// `context.max_tokens` and the threshold: an agent turn's input budget is 100 000 less
/// [`TURN_CAP`], so compaction fires at 47 952 tokens, which only the first turn's reported usage
/// reaches.
const CONTEXT: &str = "context:\n  max_tokens: 100000\n";
/// Every agent turn's input budget: `context.max_tokens` less [`TURN_CAP`].
const TURN_BUDGET: u64 = 100_000 - TURN_CAP;

/// The `input_budget` of every `inference` line, in order.
fn input_budgets(trace: &[Value]) -> Vec<Value> {
    events(trace, "inference")
        .iter()
        .map(|line| {
            line.get("input_budget")
                .cloned()
                .expect("input_budget is written")
        })
        .collect()
}
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
    manifest_with_key(endpoint, "test-key", "", hook_extra, compaction_extra)
}

/// [`manifest`] with the driver's `gateway.api_key` set to `api_key` and `config_extra` spliced
/// into `inference.driver.config` after `reasoning_effort`.
fn manifest_with_key(
    endpoint: &str,
    api_key: &str,
    config_extra: &str,
    hook_extra: &str,
    compaction_extra: &str,
) -> String {
    format!(
        "name: compaction-call\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: {api_key}\n  - name: {COMPACTOR}\n    version: 0.1.0\n    runtime: hook\n\
         {hook_extra}capabilities:\n  network:\n    allow:\n      - {endpoint}\n{CONTEXT}\
         inference:\n  transport: http\n  model: {MODEL}\n  max_tokens: {TURN_CAP}\n  driver:\n    \
         artifact: {DRIVER}\n    config:\n      reasoning_effort: high\n{config_extra}  compaction:\n    \
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
    run_compacting_traced(compaction_extra).requests
}

/// One [`run_compacting_traced`] run: the requests, the run, and the scratch `HOME` holding its
/// records.
struct Compacted {
    requests: Vec<Value>,
    run: Run,
    home: TempDir,
}

/// [`run_compacting`], keeping the run and its records.
fn run_compacting_traced(compaction_extra: &str) -> Compacted {
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
    Compacted {
        requests,
        run,
        home,
    }
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

// ── The trace names each call's settings ──────────────────────────────────────

/// Every `inference` line's caller (`origin`, `null` for the agent), `max_output_tokens` and
/// `driver_config`, in file order.
fn call_settings(trace: &[Value]) -> Vec<(Value, Value, Value)> {
    events(trace, "inference")
        .into_iter()
        .map(|line| {
            (
                line.get("origin").cloned().unwrap_or(Value::Null),
                line.get("max_output_tokens")
                    .cloned()
                    .expect("max_output_tokens on every http line"),
                line.get("driver_config")
                    .cloned()
                    .expect("driver_config on every http line"),
            )
        })
        .collect()
}

/// `turn N` for one distinct turn, `turns a, b` for several, as the Call settings rows list them.
fn turn_list(lines: &[&Value]) -> String {
    let mut turns: Vec<u64> = lines
        .iter()
        .map(|line| line["turn"].as_u64().expect("turn"))
        .collect();
    turns.sort_unstable();
    turns.dedup();
    let list = turns
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match turns.len() {
        1 => format!("turn {list}"),
        _ => format!("turns {list}"),
    }
}

#[test]
fn the_trace_records_each_calls_cap_and_driver_config() {
    let Compacted {
        requests,
        run,
        home,
    } = run_compacting_traced(BOTH_KEYS);
    let trace = run.trace();
    let settings = call_settings(&trace);
    println!("{settings:?}");
    assert_eq!(
        settings,
        [
            (
                Value::Null,
                json!(TURN_CAP),
                json!({"reasoning_effort": "high"})
            ),
            (
                json!("hook:compactor"),
                json!(2000),
                json!({"reasoning_effort": "low"})
            ),
            (
                Value::Null,
                json!(TURN_CAP),
                json!({"reasoning_effort": "high"})
            ),
        ]
    );
    // The compaction call's budget is cut from its own cap: 100 000 less 2000.
    assert_eq!(
        input_budgets(&trace),
        [json!(TURN_BUDGET), json!(98_000), json!(TURN_BUDGET)]
    );
    // What each line says is what the request at the same position carried.
    for (line, request) in settings.iter().zip(&requests) {
        assert_eq!(
            (line.1.clone(), line.2["reasoning_effort"].clone()),
            cap_and_effort(request),
            "{request:#?}"
        );
    }

    let lines = events(&trace, "inference");
    for line in &lines {
        println!("{line}");
    }
    let agent_turns = turn_list(&[lines[0], lines[2]]);
    let hook_turn = turn_list(&[lines[1]]);
    let records = common::parse_records_from_stdout(&run.stdout);
    let show = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "show"])
        .arg(records.join("trace.jsonl"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&show.stdout);
    println!("{stdout}");
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let section: Vec<&str> = stdout
        .lines()
        .skip_while(|line| !line.starts_with("── Call settings"))
        .take_while(|line| !line.is_empty())
        .collect();
    assert_eq!(
        section,
        [
            "── Call settings ────────────────────────────────".to_string(),
            format!(
                r#"agent  max_tokens {TURN_CAP}  driver_config {{"reasoning_effort":"high"}}  2 calls  {agent_turns}"#
            ),
            format!(
                r#"hook:compactor  max_tokens 2000  driver_config {{"reasoning_effort":"low"}}  1 call  {hook_turn}"#
            ),
        ]
    );
}

#[test]
fn the_trace_records_the_ceiling_and_the_agents_config_without_an_overlay() {
    // `_home` holds the records the trace is read from.
    let Compacted {
        run, home: _home, ..
    } = run_compacting_traced("");
    assert_eq!(
        input_budgets(&run.trace()),
        [
            json!(TURN_BUDGET),
            json!(100_000 - u64::from(common::FIXTURE_MAX_OUTPUT_TOKENS)),
            json!(TURN_BUDGET)
        ]
    );
    assert_eq!(
        call_settings(&run.trace()),
        [
            (
                Value::Null,
                json!(TURN_CAP),
                json!({"reasoning_effort": "high"})
            ),
            (
                json!("hook:compactor"),
                json!(common::FIXTURE_MAX_OUTPUT_TOKENS),
                json!({"reasoning_effort": "high"})
            ),
            (
                Value::Null,
                json!(TURN_CAP),
                json!({"reasoning_effort": "high"})
            ),
        ]
    );
}

#[test]
fn a_credential_in_the_driver_config_is_redacted_in_the_trace() {
    const CREDENTIAL: &str = "trace-test-credential-4be1";
    let home = summarizing_home();
    let server = common::ScriptedServer::start(compacting_script());
    let project = project(&manifest_with_key(
        &server.endpoint,
        CREDENTIAL,
        &format!("      bogus_key: {CREDENTIAL}\n"),
        "",
        "",
    ));
    let run = run(&home, &project);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(
        server.requests().is_empty(),
        "the driver refused before sending: {}",
        run.context()
    );

    let trace = run.trace();
    let lines = events(&trace, "inference");
    assert_eq!(lines.len(), 1, "{trace:#?}");
    let failed = lines[0];
    println!("{failed}");
    assert_eq!(failed["decision"], "error");
    assert_eq!(failed["error_code"], "driver_error");
    assert_eq!(failed["max_output_tokens"], json!(TURN_CAP));
    assert_eq!(failed["input_budget"], json!(TURN_BUDGET));
    assert_eq!(
        failed["driver_config"],
        json!({"bogus_key": "[redacted]", "reasoning_effort": "high"})
    );
    let text =
        fs::read_to_string(common::parse_records_from_stdout(&run.stdout).join("trace.jsonl"))
            .unwrap();
    assert!(
        !text.contains(CREDENTIAL),
        "the credential appears in trace.jsonl:\n{text}"
    );
}
