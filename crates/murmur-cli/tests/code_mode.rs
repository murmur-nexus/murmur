//! Code mode: a capsule whose `murmur.yaml` names a `runtime: tool` artifact in `code_mode.repl`
//! keeps one instance of it in the tools worker from the model's first call to it for the rest of
//! the session. A call that outruns `code_mode.detach_after_secs` returns a work id that
//! `background-command` reports on, waits for or stops; a stop, a trap, a deadline or a memory
//! limit ends the instance, and the next call starts a new one. A capsule without the block runs
//! every call in a fresh store.
//!
//! Driven through the real `mur` binary against a scripted provider, with `counter-tool`: a fixture
//! that keeps a counter in its instance's memory.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const COUNTER: &str = "local/counter-tool";
const ECHO: &str = "local/echo-tool";
const SCRIPT_PACKAGE: &str = "local/script-package";
const VERSION: &str = "0.1.0";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const KEY: &str = "sk-code-mode-canary-6c2e";
const WAIT: Duration = Duration::from_secs(240);
const INSTANCE_ENDED: &str = "The code mode instance ended; every value it held is lost, and the \
                              next call to 'counter-tool' starts a new instance.";

const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

fn mur() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// A scratch `HOME` with the fixture driver, `counter-tool` and `echo-tool` published and
/// precompiled.
struct Home {
    home: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let published = [
            common::create_driver_artifact_with_auth(
                artifacts.path(),
                ANTHROPIC,
                VERSION,
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            ),
            common::create_tool_artifact(
                artifacts.path(),
                COUNTER,
                VERSION,
                &common::fixture_path("counter-tool/tool/counter-tool.wasm"),
            ),
            common::create_tool_artifact(
                artifacts.path(),
                ECHO,
                VERSION,
                &common::fixture_path("run/components/echo-tool.wasm"),
            ),
        ];
        for artifact in &published {
            common::publish_local(&home, artifact).success();
            Command::new(mur())
                .env("HOME", home.path())
                .arg("precompile")
                .arg(artifact)
                .output()
                .unwrap();
        }
        Self { home }
    }

    /// A project whose `murmur.yaml` lists the driver, `counter-tool`, then `artifacts` (each
    /// entry's YAML), then `extra` top-level YAML.
    fn project(&self, endpoint: &str, artifacts: &str, extra: &str) -> Project {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: code-mode-capsule\nversion: 0.1.0\nartifacts:\n  - name: {ANTHROPIC}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: {KEY}\n  - name: {COUNTER}\n    version: {VERSION}\n    \
                 runtime: tool\n{artifacts}{extra}inference:\n  transport: http\n  model: \
                 test-model\n  max_turns: 12\n  driver:\n    artifact: {ANTHROPIC}\n"
            ),
        )
        .unwrap();
        Project { project, manifest }
    }

    /// A project with `manifest` written verbatim.
    fn project_from(&self, manifest_yaml: &str) -> Project {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(&manifest, manifest_yaml).unwrap();
        Project { project, manifest }
    }

    fn run(&self, project: &Project) -> Run {
        let mut child = Command::new(mur())
            .env("HOME", self.home.path())
            .current_dir(project.project.path())
            .args(["run", "--manifest", project.manifest.to_str().unwrap()])
            .args(["--task", "Count.", "--verbose"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > WAIT {
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Run::of(child.wait_with_output().unwrap())
    }

    fn doctor(&self, project: &Project) -> Run {
        Run::of(
            Command::new(mur())
                .env("HOME", self.home.path())
                .current_dir(project.project.path())
                .arg("doctor")
                .output()
                .unwrap(),
        )
    }
}

struct Project {
    project: TempDir,
    manifest: PathBuf,
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn trace(&self) -> Vec<Value> {
        let trace = common::parse_records_from_stdout(&self.stdout).join("trace.jsonl");
        assert!(trace.is_file(), "no trace.jsonl; {}", self.context());
        common::read_whole_trace(&trace)
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }

    fn assert_ok(&self) {
        assert!(self.output.status.success(), "{}", self.context());
    }
}

/// One model reply: computed from the request it answers, so a later call can name a work id an
/// earlier call returned.
type Step = Box<dyn FnMut(&Value) -> String + Send>;

/// A scripted model: each step answers one request, and [`END_TURN`] answers the one after the
/// last step. `arrivals` holds when each request arrived.
struct Model {
    server: common::ScriptedServer,
    arrivals: Arc<Mutex<Vec<Instant>>>,
}

impl Model {
    fn start(steps: Vec<Step>) -> Self {
        let limit = steps.len() + 1;
        let mut steps = steps.into_iter();
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&arrivals);
        let server = common::ScriptedServer::start_answering(limit, move |request| {
            seen.lock().unwrap().push(Instant::now());
            match steps.next() {
                Some(mut step) => step(request),
                None => END_TURN.to_string(),
            }
        });
        Self { server, arrivals }
    }

    fn endpoint(&self) -> &str {
        &self.server.endpoint
    }

    fn requests(&self) -> Vec<Value> {
        self.server.requests()
    }

    /// The text of the result of tool call `id`.
    fn result(&self, id: &str) -> String {
        common::tool_result_text(&self.requests(), id)
            .unwrap_or_else(|| panic!("no result for {id} in {:#?}", self.requests()))
    }

    /// The tool names the request at `index` offered, in array order.
    fn tool_names(&self, index: usize) -> Vec<String> {
        self.requests()[index]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect()
    }
}

/// A call to `tool` with `input`, as tool call `id`.
fn call(id: &'static str, tool: &'static str, input: Value) -> Step {
    Box::new(move |_| common::tool_use_response(id, tool, input.clone()))
}

fn counter(id: &'static str, input: Value) -> Step {
    call(id, "counter-tool", input)
}

/// A `background-command` call `id` with `operation` on the work id tool call `of` returned.
fn background(id: &'static str, operation: &'static str, of: &'static str) -> Step {
    Box::new(move |request| {
        let work_id = work_id_in(request, of);
        common::tool_use_response(
            id,
            "background-command",
            json!({"operation": operation, "work_id": work_id}),
        )
    })
}

/// The `wrk_…` id in the result of tool call `id` within `request`.
fn work_id_in(request: &Value, id: &str) -> String {
    let text = common::tool_result_text(std::slice::from_ref(request), id)
        .unwrap_or_else(|| panic!("no result for {id} in {request:#}"));
    let start = text
        .find("wrk_")
        .unwrap_or_else(|| panic!("no work id in {text}"));
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn records_of<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn index_of(trace: &[Value], pred: impl Fn(&Value) -> bool) -> usize {
    trace
        .iter()
        .position(pred)
        .unwrap_or_else(|| panic!("no matching record in {trace:#?}"))
}

fn code_mode(block: &str) -> String {
    format!("code_mode:\n  repl: {COUNTER}\n{block}")
}

/// S4: without `code_mode`, each call runs in a fresh store, nothing is recorded for code mode
/// and no `background-command` is offered.
#[test]
fn without_code_mode_each_call_gets_a_fresh_instance() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "incr"})),
        counter("t2", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(model.endpoint(), "", ""));
    run.assert_ok();
    assert_eq!(model.result("t1"), "count=1");
    assert_eq!(model.result("t2"), "count=1");
    let trace = run.trace();
    assert!(
        !trace.iter().any(|event| event["event_type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("code_mode_"))),
        "{trace:#?}"
    );
    assert!(!model
        .tool_names(0)
        .contains(&"background-command".to_string()));
}

/// S5: a session whose model never calls the code mode tool opens no instance.
#[test]
fn a_session_that_never_calls_the_tool_opens_no_instance() {
    let home = Home::new();
    let model = Model::start(Vec::new());
    let run = home.run(&home.project(model.endpoint(), "", &code_mode("")));
    run.assert_ok();
    let trace = run.trace();
    assert!(
        !trace.iter().any(|event| event["event_type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("code_mode_"))),
        "{trace:#?}"
    );
}

/// S5: the instance keeps its state from one call to the next, is opened by the first call and
/// ends with the session.
#[test]
fn the_instance_keeps_its_state_across_calls() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "incr"})),
        counter("t2", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(model.endpoint(), "", &code_mode("")));
    run.assert_ok();
    assert_eq!(model.result("t1"), "count=1");
    assert_eq!(model.result("t2"), "count=2");

    let trace = run.trace();
    let starts = records_of(&trace, "code_mode_instance_start");
    assert_eq!(starts.len(), 1, "{trace:#?}");
    assert_eq!(starts[0]["instance"], 1);
    assert_eq!(starts[0]["artifact"], COUNTER);
    assert_eq!(starts[0]["tool"], "counter-tool");
    assert!(starts[0]["task_id"].is_string(), "{}", starts[0]);
    let start = index_of(&trace, |e| e["event_type"] == "code_mode_instance_start");
    let first_call = index_of(&trace, |e| e["event_type"] == "tool_call");
    assert!(start < first_call, "{trace:#?}");

    let ends = records_of(&trace, "code_mode_instance_end");
    assert_eq!(ends.len(), 1, "{trace:#?}");
    assert_eq!(ends[0]["reason"], "session_end");
    assert_eq!(ends[0]["calls"], 2);
    assert_eq!(ends[0]["instance"], 1);
    assert!(ends[0].get("message").is_none(), "{}", ends[0]);
    let end = index_of(&trace, |e| e["event_type"] == "code_mode_instance_end");
    let session_end = index_of(&trace, |e| e["event_type"] == "session_end");
    assert!(end < session_end, "{trace:#?}");
}

/// S6: a call past `detach_after_secs` returns a work id, and `background-command wait` returns
/// its result.
#[test]
fn a_long_call_detaches_and_wait_returns_its_result() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "sleep", "secs": 4})),
        background("t2", "wait", "t1"),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &code_mode("  detach_after_secs: 1\n"),
    ));
    run.assert_ok();

    let detached = model.result("t1");
    let work_id = detached
        .split_whitespace()
        .find(|word| word.starts_with("wrk_"))
        .map(|word| word.trim_end_matches('.'))
        .unwrap_or_else(|| panic!("{detached}"));
    assert_eq!(
        detached,
        format!(
            "The call is still running and was moved to the background as {work_id}. The \
             background-command tool reports on it, waits for it or stops it by that id. \
             Stopping it ends the code mode instance, and every value the instance holds is \
             lost. A call still running when the session ends is stopped."
        )
    );
    let arrivals = model.arrivals.lock().unwrap().clone();
    let detach_took = arrivals[1] - arrivals[0];
    assert!(
        detach_took < Duration::from_secs(4),
        "the detach result came back after {detach_took:?}"
    );

    let waited = model.result("t2");
    let lines: Vec<&str> = waited.lines().collect();
    assert_eq!(lines[0], format!("work_id: {work_id}"), "{waited}");
    assert_eq!(lines[1], "tool: counter-tool", "{waited}");
    assert_eq!(lines[2], "state: finished", "{waited}");
    assert!(lines.contains(&"call_status: passed"), "{waited}");
    assert!(lines.iter().any(|line| line.starts_with("waited_secs: ")));
    assert_eq!(lines.last(), Some(&"count=1"), "{waited}");

    let trace = run.trace();
    let detaches = records_of(&trace, "code_mode_detach");
    assert_eq!(detaches.len(), 1, "{trace:#?}");
    assert_eq!(detaches[0]["work_id"], work_id);
    assert_eq!(detaches[0]["instance"], 1);
    let after_ms = detaches[0]["after_ms"].as_u64().unwrap();
    assert!((900..4000).contains(&after_ms), "{after_ms}");
    let background = records_of(&trace, "background_command");
    assert_eq!(background.len(), 1, "{trace:#?}");
    assert_eq!(background[0]["state"], "finished");
    assert_eq!(background[0]["operation"], "wait");
    assert_eq!(background[0]["artifact"], COUNTER);
    assert_eq!(background[0]["output_bytes"], 7);
    assert!(background[0].get("exit_code").is_none());
    assert!(model
        .tool_names(0)
        .contains(&"background-command".to_string()));
}

/// S7: `stop` ends the instance, and the next call starts a new one.
#[test]
fn stop_ends_the_instance_and_the_next_call_starts_a_new_one() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "incr"})),
        counter("t2", json!({"op": "sleep", "secs": 30})),
        background("t3", "stop", "t2"),
        counter("t4", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &code_mode("  detach_after_secs: 1\n"),
    ));
    run.assert_ok();
    assert_eq!(model.result("t1"), "count=1");
    let stopped = model.result("t3");
    assert!(
        stopped.lines().any(|line| line == "state: stopped"),
        "{stopped}"
    );
    assert!(
        stopped.ends_with(
            "\n\nThe call was stopped by ending the code mode instance; every value the instance \
             held is lost, and the next call to 'counter-tool' starts a new instance."
        ),
        "{stopped}"
    );
    assert_eq!(model.result("t4"), "count=1");

    let trace = run.trace();
    let ends = records_of(&trace, "code_mode_instance_end");
    assert_eq!(ends[0]["reason"], "stopped", "{trace:#?}");
    assert_eq!(ends[0]["instance"], 1);
    assert_eq!(ends[0]["calls"], 1);
    let starts = records_of(&trace, "code_mode_instance_start");
    assert_eq!(starts.len(), 2, "{trace:#?}");
    assert_eq!(starts[1]["instance"], 2);
    let stopped_at = index_of(&trace, |e| {
        e["event_type"] == "code_mode_instance_end" && e["reason"] == "stopped"
    });
    let second = index_of(&trace, |e| {
        e["event_type"] == "code_mode_instance_start" && e["instance"] == 2
    });
    assert!(stopped_at < second, "{trace:#?}");
    assert_eq!(ends.last().unwrap()["reason"], "session_end");
    assert_eq!(ends.last().unwrap()["instance"], 2);
}

/// S8: a call that arrives while a detached call runs is refused and starts nothing.
#[test]
fn a_call_during_a_detached_call_is_refused() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "sleep", "secs": 4})),
        counter("t2", json!({"op": "incr"})),
        background("t3", "wait", "t1"),
        counter("t4", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &code_mode("  detach_after_secs: 1\n"),
    ));
    run.assert_ok();
    let work_id = work_id_in(&model.requests()[1], "t1");
    let refused = model.result("t2");
    assert!(
        refused.contains(&format!(
            "'counter-tool' is running another call ({work_id}, started "
        )) && refused.contains(" s ago); the code mode instance runs one call at a time"),
        "{refused}"
    );
    let waited = model.result("t3");
    assert_eq!(waited.lines().last(), Some("count=1"), "{waited}");
    assert_eq!(model.result("t4"), "count=2");
    let trace = run.trace();
    assert_eq!(
        records_of(&trace, "code_mode_instance_start").len(),
        1,
        "{trace:#?}"
    );
}

/// S9: a trap ends the instance with the trap's text, and the next call starts a new one.
#[test]
fn a_trap_ends_the_instance() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "incr"})),
        counter("t2", json!({"op": "incr"})),
        counter("t3", json!({"op": "trap"})),
        counter("t4", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(model.endpoint(), "", &code_mode("")));
    run.assert_ok();
    assert_eq!(model.result("t2"), "count=2");
    let trapped = model.result("t3");
    assert!(trapped.contains("tool 'counter-tool' trapped"), "{trapped}");
    assert!(trapped.ends_with(INSTANCE_ENDED), "{trapped}");
    assert_eq!(model.result("t4"), "count=1");

    let trace = run.trace();
    let ends = records_of(&trace, "code_mode_instance_end");
    assert_eq!(ends[0]["reason"], "trap", "{trace:#?}");
    assert_eq!(ends[0]["calls"], 2);
    assert!(
        ends[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("trapped")),
        "{}",
        ends[0]
    );
    let starts = records_of(&trace, "code_mode_instance_start");
    assert_eq!(starts.len(), 2, "{trace:#?}");
    assert_eq!(starts[1]["instance"], 2);
}

/// S10: growing past `capabilities.limits.memory_bytes` ends the instance as a resource limit.
#[test]
fn a_memory_limit_ends_the_instance() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "grow", "mib": 128})),
        counter("t2", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &format!(
            "{}capabilities:\n  limits:\n    memory_bytes: 67108864\n",
            code_mode("")
        ),
    ));
    run.assert_ok();
    let grown = model.result("t1");
    assert!(
        grown.contains("tool 'counter-tool' exceeded its configured resource limits"),
        "{grown}"
    );
    assert!(grown.ends_with(INSTANCE_ENDED), "{grown}");
    assert_eq!(model.result("t2"), "count=1");
    let ends = run.of_type("code_mode_instance_end");
    assert_eq!(ends[0]["reason"], "resource_limit", "{ends:#?}");
    assert!(ends[0]["message"].is_string(), "{}", ends[0]);
}

/// S10: running past `capabilities.limits.deadline_seconds` ends the instance as a deadline.
#[test]
fn a_deadline_ends_the_instance() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "spin"})),
        counter("t2", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &format!(
            "{}capabilities:\n  limits:\n    deadline_seconds: 2\n",
            code_mode("")
        ),
    ));
    run.assert_ok();
    let spun = model.result("t1");
    assert!(
        spun.contains("tool 'counter-tool' exceeded its 2s execution deadline"),
        "{spun}"
    );
    assert!(spun.ends_with(INSTANCE_ENDED), "{spun}");
    assert_eq!(model.result("t2"), "count=1");
    let ends = run.of_type("code_mode_instance_end");
    assert_eq!(ends[0]["reason"], "deadline", "{ends:#?}");
}

/// S11: `stop` ends an instance whose call is computing, promptly.
#[test]
fn stop_interrupts_a_computing_call() {
    let home = Home::new();
    let model = Model::start(vec![
        counter("t1", json!({"op": "spin"})),
        background("t2", "stop", "t1"),
        counter("t3", json!({"op": "incr"})),
    ]);
    let run = home.run(&home.project(
        model.endpoint(),
        "",
        &code_mode("  detach_after_secs: 1\n"),
    ));
    run.assert_ok();
    let stopped = model.result("t2");
    assert!(
        stopped.lines().any(|line| line == "state: stopped"),
        "{stopped}"
    );
    let arrivals = model.arrivals.lock().unwrap().clone();
    let stop_took = arrivals[2] - arrivals[1];
    assert!(
        stop_took < Duration::from_secs(5),
        "the stop came back after {stop_took:?}"
    );
    assert_eq!(model.result("t3"), "count=1");
    let ends = run.of_type("code_mode_instance_end");
    assert_eq!(ends[0]["reason"], "stopped", "{ends:#?}");
}

/// S12: `tools_visible: false` offers the code mode tool, and `background-command` beside it when
/// calls can detach; the default offers every tool; the array is the same bytes on every request.
#[test]
fn tools_visible_false_offers_only_the_code_mode_tool() {
    // The capsule allows a shell binary, which needs the session's network namespace.
    if capsule_runtime::skip_without_egress_namespace(
        "tools_visible_false_offers_only_the_code_mode_tool",
    ) {
        return;
    }
    let home = Home::new();
    let echo = format!("  - name: {ECHO}\n    version: {VERSION}\n    runtime: tool\n");
    let shell = "capabilities:\n  shell:\n    allow: [cat]\n";

    let model = Model::start(vec![counter("t1", json!({"op": "incr"}))]);
    let run = home.run(&home.project(
        model.endpoint(),
        &echo,
        &format!(
            "{shell}{}",
            code_mode("  detach_after_secs: 5\n  tools_visible: false\n")
        ),
    ));
    run.assert_ok();
    assert_eq!(
        model.tool_names(0),
        vec!["background-command", "counter-tool"]
    );
    let requests = model.requests();
    assert_eq!(
        serde_json::to_string(&requests[0]["tools"]).unwrap(),
        serde_json::to_string(&requests[1]["tools"]).unwrap()
    );

    let model = Model::start(vec![counter("t1", json!({"op": "incr"}))]);
    let run = home.run(&home.project(
        model.endpoint(),
        &echo,
        &format!("{shell}{}", code_mode("  tools_visible: false\n")),
    ));
    run.assert_ok();
    assert_eq!(model.tool_names(0), vec!["counter-tool"]);

    let model = Model::start(vec![counter("t1", json!({"op": "incr"}))]);
    let run = home.run(&home.project(model.endpoint(), &echo, &format!("{shell}{}", code_mode(""))));
    run.assert_ok();
    let offered = model.tool_names(0);
    for name in ["cat", "counter-tool", "echo-tool"] {
        assert!(offered.contains(&name.to_string()), "{name}: {offered:?}");
    }
    let requests = model.requests();
    assert_eq!(
        serde_json::to_string(&requests[0]["tools"]).unwrap(),
        serde_json::to_string(&requests[1]["tools"]).unwrap()
    );
}

/// S2: every invalid `code_mode` block is refused by `mur run` and `mur doctor` with `E-MAN-004`,
/// naming the field.
#[test]
fn invalid_blocks_are_refused_by_run_and_doctor() {
    let home = Home::new();
    let endpoint = "http://127.0.0.1:9";
    let agent = |block: &str| home.project(endpoint, "", block);
    let cases = [
        (agent("code_mode: {}\n"), "code_mode.repl"),
        (agent("code_mode:\n  repl: '  '\n"), "code_mode.repl"),
        (agent("code_mode:\n  repl: 7\n"), "code_mode.repl"),
        (
            agent(&code_mode("  detach_after_secs: -1\n")),
            "code_mode.detach_after_secs",
        ),
        (
            agent(&code_mode("  tools_visible: maybe\n")),
            "code_mode.tools_visible",
        ),
        (
            agent("code_mode:\n  repl: local/not-declared\n"),
            "code_mode.repl",
        ),
        (agent(&format!("code_mode:\n  repl: {ANTHROPIC}\n")), "code_mode.repl"),
        (
            home.project_from(&format!(
                "name: script\nversion: 0.1.0\nartifacts:\n  - name: {COUNTER}\n    version: \
                 {VERSION}\n    runtime: tool\n{}",
                code_mode("")
            )),
            "code_mode",
        ),
    ];
    for (project, field) in &cases {
        let manifest = fs::read_to_string(&project.manifest).unwrap();
        let expected = format!("error[E-MAN-004]: murmur.yaml: invalid code_mode config for '{field}': ");
        let run = home.run(project);
        assert!(!run.output.status.success(), "{manifest}\n{}", run.context());
        assert!(run.stderr.contains(&expected), "{manifest}\n{}", run.context());
        let doctor = home.doctor(project);
        assert!(!doctor.output.status.success(), "{manifest}\n{}", doctor.context());
        assert!(
            doctor.stderr.contains(&expected) || doctor.stdout.contains(&expected),
            "{manifest}\n{}",
            doctor.context()
        );
    }
}

/// S3: a `repl` whose package is a script capsule's is refused by `mur run` before any task, and
/// reported by `mur doctor` as an error.
#[test]
fn a_script_capsule_package_named_by_repl_is_refused() {
    let home = Home::new();
    common::publish_to_store(
        &home.home.path().join(".murmur").join("artifacts"),
        SCRIPT_PACKAGE,
        VERSION,
        "capsule",
        "runtime: capsule\n",
        Some((
            "capsule.wasm",
            &common::fixture_path("counter-tool/tool/counter-tool.wasm"),
        )),
    );
    let model = Model::start(Vec::new());
    let project = home.project(
        model.endpoint(),
        &format!("  - name: {SCRIPT_PACKAGE}\n    version: {VERSION}\n    runtime: tool\n"),
        &format!("code_mode:\n  repl: {SCRIPT_PACKAGE}\n"),
    );
    let expected = format!(
        "error[E-MAN-004]: murmur.yaml: invalid code_mode config for 'code_mode.repl': \
         '{SCRIPT_PACKAGE}' is a script capsule's package (its murmur.yaml declares runtime: \
         capsule)"
    );
    let run = home.run(&project);
    assert!(!run.output.status.success(), "{}", run.context());
    assert!(run.stderr.contains(&expected), "{}", run.context());
    assert!(model.requests().is_empty(), "a task ran: {:#?}", model.requests());

    let doctor = home.doctor(&project);
    assert_eq!(doctor.output.status.code(), Some(1), "{}", doctor.context());
    assert!(
        doctor.stdout.contains("E-MAN-004") || doctor.stderr.contains("E-MAN-004"),
        "{}",
        doctor.context()
    );
    assert!(
        doctor
            .stdout
            .contains(&format!("'{SCRIPT_PACKAGE}' is a script capsule's package")),
        "{}",
        doctor.context()
    );
}
