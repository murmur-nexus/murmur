//! `murmur:bus`: a program running in the code mode tool's instance lists the capsule's
//! operations, calls one, or calls several at once, under the checks the model's own call to each
//! meets. Each call is recorded as the model's call to that tool is, marked with the code mode
//! call it was made in; any other artifact importing the bus is refused at launch (`E-CAP-022`).
//!
//! Driven through the real `mur` binary against a scripted provider, with two fixtures built from
//! one crate: `bus-repl`, a code mode tool whose input names a bus operation to perform, and
//! `bus-echo`, an ordinary tool that sleeps and echoes a tag.

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

use common::hook_wat::{create_hook_zip, tool_name_deny_hook_wasm};

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const REPL: &str = "local/bus-repl";
const REPL_COPY: &str = "local/bus-repl-copy";
const ECHO: &str = "local/bus-echo";
const VERSION: &str = "0.1.0";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const KEY: &str = "sk-code-mode-bus-canary-41d7";
const WAIT: Duration = Duration::from_secs(240);
/// The capsule-wide grant that offers the runtime-provided `session-status` tool.
const SESSION_STATUS_GRANT: &str = "capabilities:\n  session_status:\n    read: true\n";
const ECHO_DESCRIPTION: &str = "Sleeps sleep_ms milliseconds, then echoes its tag.";
const ECHO_MANIFEST: &str = "description: Sleeps sleep_ms milliseconds, then echoes its tag.\n\
     input_schema: '{\"type\":\"object\",\"properties\":{\"tag\":{\"type\":\"string\"},\
     \"sleep_ms\":{\"type\":\"integer\"}},\"required\":[\"tag\"]}'\n";

const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

fn mur() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// A scratch `HOME` with the fixture driver, `bus-repl` (also as `bus-repl-copy`) and `bus-echo`
/// published and precompiled, and `hooks` published beside them.
struct Home {
    home: TempDir,
}

impl Home {
    fn new() -> Self {
        Self::with_hooks(&[])
    }

    fn with_hooks(hooks: &[PathBuf]) -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let repl = common::fixture_path("bus-probe/tool/bus-repl.wasm");
        let mut published = vec![
            common::create_driver_artifact_with_auth(
                artifacts.path(),
                ANTHROPIC,
                VERSION,
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            ),
            common::create_tool_artifact(artifacts.path(), REPL, VERSION, &repl),
            common::create_tool_artifact(artifacts.path(), REPL_COPY, VERSION, &repl),
            common::create_tool_artifact_with_auth(
                artifacts.path(),
                ECHO,
                VERSION,
                &common::fixture_path("bus-probe/tool/bus-echo.wasm"),
                ECHO_MANIFEST,
            ),
        ];
        published.extend(hooks.iter().cloned());
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

    /// A project whose `murmur.yaml` lists the driver, then `artifacts` (each entry's YAML), then
    /// `extra` top-level YAML.
    fn project(&self, endpoint: &str, artifacts: &str, extra: &str) -> Project {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: code-mode-bus-capsule\nversion: 0.1.0\nartifacts:\n  - name: {ANTHROPIC}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: {KEY}\n{artifacts}{extra}inference:\n  transport: \
                 http\n  model: test-model\n  max_turns: 12\n  driver:\n    artifact: \
                 {ANTHROPIC}\n"
            ),
        )
        .unwrap();
        Project { project, manifest }
    }

    /// The base capsule: `bus-repl` and `bus-echo` as tools, `code_mode.repl: local/bus-repl`,
    /// `more` further artifact entries and `extra` further YAML (`code_mode` keys first).
    fn base(&self, endpoint: &str, more: &str, code_mode_keys: &str, extra: &str) -> Project {
        self.project(
            endpoint,
            &format!("{}{}{more}", tool(REPL), tool(ECHO)),
            &format!("code_mode:\n  repl: {REPL}\n{code_mode_keys}{extra}"),
        )
    }

    fn run(&self, project: &Project) -> Run {
        let mut child = Command::new(mur())
            .env("HOME", self.home.path())
            .current_dir(project.project.path())
            .args(["run", "--manifest", project.manifest.to_str().unwrap()])
            .args(["--task", "Use the bus.", "--verbose"])
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

    fn trace_show(&self, trace: &std::path::Path) -> Run {
        Run::of(
            Command::new(mur())
                .env("HOME", self.home.path())
                .args(["trace", "show"])
                .arg(trace)
                .output()
                .unwrap(),
        )
    }
}

fn tool(name: &str) -> String {
    format!("  - name: {name}\n    version: {VERSION}\n    runtime: tool\n")
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

    fn trace_path(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout).join("trace.jsonl")
    }

    fn trace(&self) -> Vec<Value> {
        let trace = self.trace_path();
        assert!(trace.is_file(), "no trace.jsonl; {}", self.context());
        common::read_whole_trace(&trace)
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }

    fn assert_ok(&self) {
        assert!(self.output.status.success(), "{}", self.context());
    }
}

type Step = Box<dyn FnMut(&Value) -> String + Send>;

/// A scripted model: each step answers one request, and [`END_TURN`] the one after the last.
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

    /// The unfenced text of the result of tool call `id`.
    fn result(&self, id: &str) -> String {
        common::tool_result_text(&self.requests(), id)
            .unwrap_or_else(|| panic!("no result for {id} in {:#?}", self.requests()))
    }

    /// The JSON `bus-repl` returned as tool call `id`'s data.
    fn data(&self, id: &str) -> Value {
        let text = self.result(id);
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    /// The tool array the request at `index` offered.
    fn tools(&self, index: usize) -> Vec<Value> {
        self.requests()[index]["tools"].as_array().unwrap().clone()
    }
}

/// A call to `bus-repl` with `input`, as tool call `id`.
fn repl(id: &'static str, input: Value) -> Step {
    Box::new(move |_| common::tool_use_response(id, "bus-repl", input.clone()))
}

fn bus_calls(trace: &[Value]) -> Vec<&Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == "tool_call" && event["origin"] == "code_mode")
        .collect()
}

fn tool_calls_named<'a>(trace: &'a [Value], name: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == "tool_call" && event["tool_name"] == name)
        .collect()
}

/// The `tool_call_id` of the model's own `bus-repl` call `id`.
fn repl_record<'a>(trace: &'a [Value], id: &str) -> &'a Value {
    trace
        .iter()
        .find(|event| event["event_type"] == "tool_call" && event["tool_call_id"] == id)
        .unwrap_or_else(|| panic!("no tool_call {id} in {trace:#?}"))
}

/// The refusal text of a `{"refused": …}` answer.
fn refused(answer: &Value) -> &str {
    answer["refused"]
        .as_str()
        .unwrap_or_else(|| panic!("not a refusal: {answer}"))
}

/// S2: `list` returns every operation the model could call except the code mode tool and the
/// three runtime-provided tools the bus does not serve, with the model's descriptions and schemas,
/// and writes no record.
#[test]
fn list_names_the_capsules_operations() {
    let home = Home::new();
    let model = Model::start(vec![repl("t1", json!({"op": "list"}))]);
    let grants = "capabilities:\n  plan:\n    submit: true\n  session_status:\n    read: true\n";
    let run = home.run(&home.base(model.endpoint(), "", "", grants));
    run.assert_ok();

    let listed = model.data("t1");
    let listed = listed.as_array().unwrap_or_else(|| panic!("{listed}"));
    let names: Vec<&str> = listed
        .iter()
        .map(|operation| operation["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"bus-echo"), "{names:?}");
    assert!(names.contains(&"session-status"), "{names:?}");
    for absent in [
        "bus-repl",
        "pull-artifact",
        "remove-artifact",
        "submit-plan",
    ] {
        assert!(!names.contains(&absent), "{absent} in {names:?}");
    }
    let offered = model.tools(0);
    assert!(
        offered.iter().any(|tool| tool["name"] == "submit-plan"),
        "the grant offers submit-plan to the model: {offered:#?}"
    );
    let echo = listed
        .iter()
        .find(|operation| operation["name"] == "bus-echo")
        .unwrap();
    assert_eq!(echo["description"], ECHO_DESCRIPTION);
    let offered_echo = offered
        .iter()
        .find(|tool| tool["name"] == "bus-echo")
        .unwrap();
    let schema = offered_echo
        .get("input_schema")
        .or_else(|| offered_echo.get("parameters"))
        .unwrap();
    assert_eq!(&echo["input_schema"], schema);
    assert!(bus_calls(&run.trace()).is_empty());
}

/// S2: with `tools_visible: false` the model is offered only the code mode tool, and `list`
/// still names the capsule's other tools.
#[test]
fn list_ignores_tools_visible() {
    let home = Home::new();
    let model = Model::start(vec![repl("t1", json!({"op": "list"}))]);
    let run = home.run(&home.base(model.endpoint(), "", "  tools_visible: false\n", ""));
    run.assert_ok();
    let offered: Vec<String> = model
        .tools(0)
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(offered, ["bus-repl"]);
    let listed = model.data("t1");
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|operation| operation["name"] == "bus-echo"),
        "{listed}"
    );
    assert!(bus_calls(&run.trace()).is_empty());
}

/// S3: a call returns the fenced result and is recorded under the code mode call; each check
/// refuses with its reason and text, and a refused call is recorded with it.
#[test]
fn call_runs_an_operation_and_refuses_with_each_check() {
    let home = Home::new();
    let call = |id: &'static str, name: &str, input: Value| {
        repl(id, json!({"op": "call", "name": name, "input": input}))
    };
    let model = Model::start(vec![
        call("t1", "bus-echo", json!({"tag": "a"})),
        call("t2", "session-status", json!({})),
        call("t3", "nope", json!({})),
        call("t4", "bus-repl", json!({})),
        call("t5", "pull-artifact", json!({"artifact": "x"})),
        repl(
            "t6",
            json!({"op": "call_text", "name": "bus-echo", "text": "{not json"}),
        ),
        call("t7", "bus-echo", json!({})),
        call("t8", "bus-echo", json!({"tag": "b"})),
    ]);
    let run = home.run(&home.base(model.endpoint(), "", "", SESSION_STATUS_GRANT));
    run.assert_ok();

    let ok = model.data("t1");
    let result = &ok["ok"];
    assert_eq!(result["status"], "passed", "{ok}");
    assert!(result["summary"].is_null(), "{ok}");
    // The program is handed the fenced text; the model reads it inside `bus-repl`'s own fence,
    // which neutralises the inner markers.
    let data = result["data"].as_str().unwrap();
    assert!(
        data.starts_with("<!MURMUR-NEUTRALISED!untrusted-content source=tool:bus-echo>\ntag=a\n"),
        "{data}"
    );
    assert_eq!(model.data("t2")["ok"]["status"], "passed");

    assert_eq!(
        refused(&model.data("t3")),
        "'nope' is not an operation this program may call; list() returns every operation it \
         may call"
    );
    assert_eq!(
        refused(&model.data("t4")),
        "'bus-repl' is the code mode tool this program runs in, which a program cannot call"
    );
    assert_eq!(
        refused(&model.data("t5")),
        "'pull-artifact' is not served to a code mode program"
    );
    let not_json = refused(&model.data("t6")).to_string();
    assert!(
        not_json.starts_with("the input for 'bus-echo' is not JSON: "),
        "{not_json}"
    );
    let missing = refused(&model.data("t7")).to_string();
    assert!(missing.contains("tag"), "{missing}");

    let trace = run.trace();
    let refused_input = trace
        .iter()
        .find(|event| event["event_type"] == "tool_input_refused")
        .unwrap_or_else(|| panic!("no tool_input_refused in {trace:#?}"));
    assert_eq!(refused_input["tool_name"], "bus-echo");

    let bus = bus_calls(&trace);
    let of = |repl_id: &str| {
        let repl_call = repl_record(&trace, repl_id);
        assert_eq!(repl_call["tool_name"], "bus-repl");
        assert!(repl_call.get("origin").is_none(), "{repl_call}");
        bus.iter()
            .copied()
            .filter(|event| event["repl_call_id"] == repl_id)
            .collect::<Vec<_>>()
    };
    for (repl_id, tool) in [("t1", "bus-echo"), ("t2", "session-status")] {
        let calls = of(repl_id);
        assert_eq!(calls.len(), 1, "{calls:#?}");
        assert_eq!(calls[0]["tool_name"], tool);
        assert_eq!(calls[0]["status"], "ok");
        assert!(calls[0].get("refusal").is_none(), "{}", calls[0]);
        assert!(calls[0]["tool_call_id"].is_null(), "{}", calls[0]);
        // A program's call through the bus is not journaled; the model's own call is.
        assert!(calls[0].get("effect_id").is_none(), "{}", calls[0]);
        let repl_call = repl_record(&trace, repl_id);
        let effect_id = repl_call["effect_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the model's own code mode call is journaled: {repl_call}"));
        assert!(trace
            .iter()
            .any(|event| event["event_type"] == "effect_started"
                && event["effect_id"] == effect_id
                && event["tool_name"] == "bus-repl"));
    }
    assert!(
        trace
            .iter()
            .filter(|event| event["event_type"] == "effect_started")
            .all(|event| event["tool_name"] == "bus-repl"),
        "only the model's own calls are journaled: {trace:#?}"
    );
    for (repl_id, tool, reason) in [
        ("t3", "nope", "not-listed"),
        ("t4", "bus-repl", "code-mode-tool"),
        ("t5", "pull-artifact", "not-served"),
        ("t6", "bus-echo", "input-not-json"),
        ("t7", "bus-echo", "required-fields"),
    ] {
        let calls = of(repl_id);
        assert_eq!(calls.len(), 1, "{repl_id}: {calls:#?}");
        assert_eq!(calls[0]["tool_name"], tool);
        assert_eq!(calls[0]["status"], "error");
        assert_eq!(calls[0]["refusal"], reason, "{}", calls[0]);
        assert!(calls[0]["error"].is_string(), "{}", calls[0]);
    }
    let ran_echo = tool_calls_named(&trace, "bus-echo")
        .into_iter()
        .filter(|event| event["status"] == "ok")
        .count();
    assert_eq!(ran_echo, 2, "{trace:#?}");
}

/// S4: an `on-tool-call` policy hook refuses a bus call as it refuses the model's.
#[test]
fn a_policy_hook_denies_a_bus_call() {
    if common::skip_without_host_support("a_policy_hook_denies_a_bus_call") {
        return;
    }
    let hooks = TempDir::new().unwrap();
    let hook = create_hook_zip(
        hooks.path(),
        "local/echo-policy",
        "on-tool-call",
        "deny",
        &tool_name_deny_hook_wasm("bus-echo", "echo is off"),
    );
    let home = Home::with_hooks(&[hook]);
    let model = Model::start(vec![repl(
        "t1",
        json!({"op": "call", "name": "bus-echo", "input": {"tag": "a"}}),
    )]);
    let run = home.run(&home.base(
        model.endpoint(),
        "  - name: local/echo-policy\n    version: 0.1.0\n    runtime: hook\n",
        "",
        "",
    ));
    run.assert_ok();
    assert_eq!(
        refused(&model.data("t1")),
        "Refused by policy hook 'echo-policy': echo is off\n\nThis call did not run. Retrying it \
         unchanged will be refused again."
    );
    let trace = run.trace();
    let denied: Vec<&Value> = trace
        .iter()
        .filter(|event| event["event_type"] == "call_denied")
        .collect();
    assert_eq!(denied.len(), 1, "{trace:#?}");
    assert_eq!(denied[0]["target"], "bus-echo");
    let bus = bus_calls(&trace);
    assert_eq!(bus.len(), 1, "{bus:#?}");
    assert_eq!(bus[0]["refusal"], "on-tool-call");
    assert!(!tool_calls_named(&trace, "bus-echo")
        .iter()
        .any(|event| event["status"] == "ok"));
}

/// S5: a batch runs its calls at once and answers in input order, refusals in place; an empty
/// batch answers `[]` and records nothing.
#[test]
fn batch_runs_calls_at_once_in_order() {
    let home = Home::new();
    let echo = |tag: &str, sleep_ms: u64| json!({"name": "bus-echo", "input": {"tag": tag, "sleep_ms": sleep_ms}});
    let model = Model::start(vec![
        repl(
            "t1",
            json!({"op": "batch", "calls": [echo("a", 3000), echo("b", 3000), echo("c", 3000)]}),
        ),
        repl(
            "t2",
            json!({"op": "batch", "calls": [echo("x", 0), {"name": "nope", "input": {}}, echo("y", 0)]}),
        ),
        repl("t3", json!({"op": "batch", "calls": []})),
    ]);
    let run = home.run(&home.base(model.endpoint(), "", "", ""));
    run.assert_ok();

    let batch = model.data("t1");
    let elapsed = batch["elapsed_ms"].as_u64().unwrap();
    assert!((3000..6000).contains(&elapsed), "{batch}");
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    for (result, tag) in results.iter().zip(["a", "b", "c"]) {
        assert_eq!(result["ok"]["status"], "passed", "{result}");
        let data = result["ok"]["data"].as_str().unwrap();
        assert!(data.contains(&format!("tag={tag}")), "{data}");
    }

    let mixed = model.data("t2");
    let mixed = mixed["results"].as_array().unwrap();
    assert!(mixed[0]["ok"]["data"].as_str().unwrap().contains("tag=x"));
    assert!(refused(&mixed[1]).starts_with("'nope' is not an operation"));
    assert!(mixed[2]["ok"]["data"].as_str().unwrap().contains("tag=y"));

    let empty = model.data("t3");
    assert_eq!(empty["results"], json!([]), "{empty}");

    let trace = run.trace();
    let bus = bus_calls(&trace);
    let first: Vec<&&Value> = bus.iter().filter(|e| e["repl_call_id"] == "t1").collect();
    assert_eq!(first.len(), 3, "{bus:#?}");
    let second: Vec<&&Value> = bus.iter().filter(|e| e["repl_call_id"] == "t2").collect();
    assert_eq!(second.len(), 3, "{bus:#?}");
    assert_eq!(
        second
            .iter()
            .filter(|e| e["refusal"] == "not-listed")
            .count(),
        1
    );
    assert!(!bus.iter().any(|e| e["repl_call_id"] == "t3"), "{bus:#?}");
}

/// S8: `mur trace show` prints each bus call under the code mode call it was made in, and counts
/// them.
#[test]
fn trace_show_nests_bus_calls() {
    let home = Home::new();
    let echo = |tag: &str| json!({"name": "bus-echo", "input": {"tag": tag}});
    let model = Model::start(vec![repl(
        "t1",
        json!({"op": "batch", "calls": [echo("a"), {"name": "nope", "input": {}}, echo("b")]}),
    )]);
    let run = home.run(&home.base(model.endpoint(), "", "", ""));
    run.assert_ok();
    let show = home.trace_show(&run.trace_path());
    assert!(show.output.status.success(), "{}", show.context());
    let lines: Vec<&str> = show.stdout.lines().collect();
    let turn = lines
        .iter()
        .position(|line| line.starts_with("  turn ") && line.contains("bus-repl"))
        .unwrap_or_else(|| panic!("{}", show.stdout));
    // In file order: the refusal is recorded as the batch is checked, each answered call as it
    // completes.
    let nested = &lines[turn + 1..turn + 4];
    assert!(
        nested[0].starts_with("      ↳ nope ") && nested[0].ends_with("  refused: not-listed"),
        "{nested:?}"
    );
    for line in &nested[1..] {
        assert!(line.starts_with("      ↳ bus-echo "), "{nested:?}");
        assert!(!line.contains("refused:"), "{nested:?}");
    }
    assert!(
        lines.get(turn + 4).is_none_or(|line| !line.contains('↳')),
        "{lines:?}"
    );
    let section = lines
        .iter()
        .position(|line| line.contains("Tool calls"))
        .unwrap_or_else(|| panic!("{}", show.stdout));
    let count = lines[section..]
        .iter()
        .find(|line| line.starts_with("count:"))
        .unwrap_or_else(|| panic!("{}", show.stdout));
    assert!(count.ends_with("  code mode: 3"), "{count}");
}

/// S9: a detach that falls due while a bus call runs takes effect once it is answered, and the
/// call is not dropped; a request after the detach is refused and recorded.
#[test]
fn detach_waits_for_the_bus_call_and_refuses_after() {
    let home = Home::new();
    let background = |id: &'static str, of: &'static str| -> Step {
        Box::new(move |request| {
            let text = common::tool_result_text(std::slice::from_ref(request), of)
                .unwrap_or_else(|| panic!("no result for {of}"));
            let start = text.find("wrk_").unwrap_or_else(|| panic!("{text}"));
            let work_id: String = text[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            common::tool_use_response(
                id,
                "background-command",
                json!({"operation": "wait", "work_id": work_id}),
            )
        })
    };
    let model = Model::start(vec![
        repl(
            "t1",
            json!({"op": "call_then_sleep", "name": "bus-echo", "input": {"tag": "a", "sleep_ms": 3000}, "sleep_ms": 3000}),
        ),
        background("t2", "t1"),
        repl(
            "t3",
            json!({"op": "sleep_then_call", "sleep_ms": 3000, "name": "bus-echo", "input": {"tag": "b"}}),
        ),
        background("t4", "t3"),
    ]);
    let run = home.run(&home.base(model.endpoint(), "", "  detach_after_secs: 1\n", ""));
    run.assert_ok();

    let arrivals = model.arrivals.lock().unwrap().clone();
    let first_detach = arrivals[1] - arrivals[0];
    assert!(
        first_detach >= Duration::from_secs(3),
        "the detach came back after {first_detach:?}, while the bus call ran"
    );
    assert!(
        model.result("t1").contains("wrk_"),
        "{}",
        model.result("t1")
    );
    let waited = model.result("t2");
    assert!(waited.contains("state: finished"), "{waited}");
    assert!(waited.contains("\"ok\""), "{waited}");
    assert!(waited.contains("tag=a"), "{waited}");

    let second_detach = arrivals[3] - arrivals[2];
    assert!(
        second_detach < Duration::from_secs(3),
        "the detach came back after {second_detach:?}"
    );
    let detached = model.result("t3");
    let start = detached.find("wrk_").unwrap();
    let work_id: String = detached[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let waited = model.result("t4");
    assert!(
        waited.contains(&format!(
            "'bus-echo' was not called: the 'bus-repl' call this program runs in was moved to the \
             background as {work_id}, and the bus answers only while that call is in the \
             foreground"
        )),
        "{waited}"
    );

    let trace = run.trace();
    let bus = bus_calls(&trace);
    let refused_after: Vec<&&Value> = bus.iter().filter(|e| e["refusal"] == "detached").collect();
    assert_eq!(refused_after.len(), 1, "{bus:#?}");
    assert_eq!(refused_after[0]["repl_call_id"], "t3");
    let echoes: Vec<&Value> = tool_calls_named(&trace, "bus-echo")
        .into_iter()
        .filter(|event| event["status"] == "ok")
        .collect();
    assert_eq!(echoes.len(), 1, "bus-echo ran once, for tag a: {echoes:#?}");
}

/// S10: a plan step's call to the code mode tool runs, and its program's bus request is refused
/// and recorded.
#[test]
fn a_plan_step_is_not_answered() {
    let home = Home::new();
    let plan = json!({
        "id": "p1",
        "steps": [{
            "id": "s1",
            "tool": "bus-repl",
            "input": {"op": "call", "name": "bus-echo", "input": {"tag": "p"}}
        }]
    });
    let model = Model::start(vec![Box::new(move |_| {
        common::tool_use_response("t1", "submit-plan", json!({"plan": plan.clone()}))
    })]);
    let run = home.run(&home.base(
        model.endpoint(),
        "",
        "",
        "capabilities:\n  plan:\n    submit: true\n",
    ));
    run.assert_ok();
    let report = model.result("t1");
    assert!(
        report.contains(
            "'bus-echo' was not called: the bus answers only while the model's own 'bus-repl' \
             call is in progress on inference.transport: http, and this 'bus-repl' call is a \
             plan step's"
        ),
        "{report}"
    );
    let trace = run.trace();
    let bus = bus_calls(&trace);
    assert_eq!(bus.len(), 1, "{bus:#?}");
    assert_eq!(bus[0]["refusal"], "not-answered");
    assert!(bus[0]["repl_call_id"].is_null(), "{}", bus[0]);
    assert!(!tool_calls_named(&trace, "bus-echo")
        .iter()
        .any(|event| event["status"] == "ok"));
}

/// S7: an artifact importing `murmur:bus` that `code_mode.repl` does not name is refused by
/// `mur run` and `mur doctor` with `E-CAP-022`, and no task starts.
#[test]
fn a_bus_import_is_refused_at_launch_unless_code_mode_names_it() {
    let home = Home::new();
    let cases = [
        (
            "no code_mode",
            home.project(
                "http://127.0.0.1:9",
                &format!("{}{}", tool(REPL), tool(ECHO)),
                "",
            ),
            REPL,
        ),
        (
            "a copy",
            home.base("http://127.0.0.1:9", &tool(REPL_COPY), "", ""),
            REPL_COPY,
        ),
    ];
    for (case, project, artifact) in &cases {
        let run = home.run(project);
        assert!(!run.output.status.success(), "{case}: {}", run.context());
        let expected = format!(
            "error[E-CAP-022]: artifact '{artifact}' imports murmur:bus but is not the artifact \
             code_mode.repl names"
        );
        assert!(run.stderr.contains(&expected), "{case}: {}", run.context());
        assert!(
            !run.stdout.contains("task_start"),
            "{case}: {}",
            run.context()
        );

        let doctor = home.doctor(project);
        assert!(
            !doctor.output.status.success(),
            "{case}: {}",
            doctor.context()
        );
        let all = format!("{}{}", doctor.stdout, doctor.stderr);
        assert!(all.contains("E-CAP-022"), "{case}: {all}");
        assert!(all.contains(artifact), "{case}: {all}");
        assert!(
            !all.trim_end().to_lowercase().ends_with("all checks passed"),
            "{case}: {all}"
        );
    }
}

/// S7: a capsule without `code_mode` declaring only `bus-echo` runs, and its call succeeds.
#[test]
fn refused_at_launch_spares_a_tool_without_the_bus() {
    let home = Home::new();
    let model = Model::start(vec![Box::new(|_| {
        common::tool_use_response("t1", "bus-echo", json!({"tag": "plain"}))
    })]);
    let run = home.run(&home.project(model.endpoint(), &tool(ECHO), ""));
    run.assert_ok();
    assert_eq!(model.result("t1"), "tag=plain");
    let trace = run.trace();
    assert!(bus_calls(&trace).is_empty());
    let call = &tool_calls_named(&trace, "bus-echo")[0];
    for key in ["origin", "repl_call_id", "refusal"] {
        assert!(call.get(key).is_none(), "{key} in {call}");
    }
}

/// S7: the driver and hook staging arms refuse a component importing `murmur:bus` with
/// `E-CAP-022`, as the tool arm does: `bus-repl` published as a `runtime: driver` and as a
/// `runtime: hook` is refused at launch.
#[test]
fn refused_at_launch_is_a_driver_and_a_hook_importing_the_bus() {
    let packed = TempDir::new().unwrap();
    let repl = common::fixture_path("bus-probe/tool/bus-repl.wasm");
    let driver = common::create_driver_artifact_with_auth(
        packed.path(),
        "local/bus-driver",
        VERSION,
        &repl,
        &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
    );
    let hook = create_hook_zip(
        packed.path(),
        "local/bus-hook",
        "on-tool-call",
        "deny",
        &fs::read(&repl).unwrap(),
    );
    let home = Home::with_hooks(&[driver, hook]);
    let cases = [("local/bus-driver", "driver"), ("local/bus-hook", "hook")];
    for (artifact, runtime) in cases {
        let project = home.project(
            "http://127.0.0.1:9",
            &format!("  - name: {artifact}\n    version: {VERSION}\n    runtime: {runtime}\n"),
            "",
        );
        let run = home.run(&project);
        assert!(
            !run.output.status.success(),
            "{artifact}: {}",
            run.context()
        );
        let expected = format!(
            "error[E-CAP-022]: artifact '{artifact}' imports murmur:bus but is not the artifact \
             code_mode.repl names"
        );
        assert!(
            run.stderr.contains(&expected),
            "{artifact}: {}",
            run.context()
        );
    }
}
