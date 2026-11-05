//! A WASM tool's programs through `murmur:process`, driven through the real `mur` binary on a
//! scripted model with `transport: http`.
//!
//! The tool is the committed `process-probe` fixture, which runs one `murmur:process` operation
//! per step and returns its output as JSON. Each scenario runs under `containment: scoped` and
//! again under `containment: sealed` where the host reaches it. Scratch directories live under
//! `CARGO_TARGET_TMPDIR`, never `/tmp`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use assert_cmd::Command;
use murmur_artifact::ContainmentClass;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const PROBE: &str = "process-probe";
const PROBE_B: &str = "process-probe-b";
const VERSION: &str = "0.1.0";
const HOOK: &str = "no-programs";

/// The tiers a scenario runs at on this host: `scoped` always, `sealed` when reachable.
fn tiers(test_name: &str) -> Vec<&'static str> {
    let mut tiers = vec!["scoped"];
    let achieved = capsule_runtime::detect_containment_decision(ContainmentClass::Sealed)
        .0
        .achieved();
    if achieved >= ContainmentClass::Sealed {
        tiers.push("sealed");
    } else {
        eprintln!("[SKIP-HOST] {test_name} (sealed): this host reaches {achieved}, the variant needs sealed");
    }
    tiers
}

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    project: PathBuf,
    artifacts: PathBuf,
}

/// One scripted call: the call id, the tool, and its input. A string `"$HANDLE:<id>"` anywhere in
/// the input is replaced by the `handle` the result of call `<id>` returned.
struct Call {
    id: &'static str,
    tool: &'static str,
    input: Value,
}

fn call(id: &'static str, input: Value) -> Call {
    Call {
        id,
        tool: PROBE,
        input,
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let project = base.join("project");
        let artifacts = base.join("artifacts");
        for dir in [&home, &project, &artifacts] {
            fs::create_dir_all(dir).unwrap();
        }
        let fixture = Self {
            _root: root,
            home,
            project,
            artifacts,
        };
        fixture.write_manifest("http://127.0.0.1:9", "scoped", "", false, false);
        let driver = common::create_driver_artifact(
            &fixture.artifacts,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        fixture.install(&driver);
        for name in [PROBE, PROBE_B] {
            let probe = common::create_tool_artifact(
                &fixture.artifacts,
                &format!("local/{name}"),
                VERSION,
                &common::fixture_path("process-probe/tool/process-probe.wasm"),
            );
            fixture.install(&probe);
        }
        let hook = common::hook_wat::create_hook_zip(
            &fixture.artifacts,
            &format!("local/{HOOK}"),
            "on-shell",
            "deny",
            &common::hook_wat::deny_hook_wasm("on-shell", "no programs today"),
        );
        fixture.install(&hook);
        fixture
    }

    fn install(&self, artifact: &Path) {
        self.mur()
            .args(["install", artifact.to_str().unwrap(), "--no-precompile"])
            .assert()
            .success();
    }

    fn manifest(&self) -> PathBuf {
        self.project.join("murmur.yaml")
    }

    /// The capsule at `containment`, with `extra` lines under `capabilities:`, the second probe
    /// when `probe_b`, and the deny hook when `hook`.
    fn write_manifest(
        &self,
        endpoint: &str,
        containment: &str,
        extra: &str,
        probe_b: bool,
        hook: bool,
    ) {
        let mut artifacts = format!(
            "  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
             gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n  - name: \
             local/{PROBE}\n    version: {VERSION}\n    runtime: tool\n"
        );
        if probe_b {
            artifacts.push_str(&format!(
                "  - name: local/{PROBE_B}\n    version: {VERSION}\n    runtime: tool\n"
            ));
        }
        if hook {
            artifacts.push_str(&format!(
                "  - name: local/{HOOK}\n    version: 0.1.0\n    runtime: hook\n"
            ));
        }
        fs::write(
            self.manifest(),
            format!(
                "name: session-processes-capsule\nversion: 0.0.1\nartifacts:\n{artifacts}\
                 capabilities:\n  containment: {containment}\n  network:\n    allow:\n      - \
                 {endpoint}\n{extra}inference:\n  transport: http\n  model: test-model\n  \
                 driver:\n    artifact: {DRIVER}\n"
            ),
        )
        .unwrap();
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command.env("HOME", &self.home).current_dir(&self.project);
        command
    }

    /// One `mur run`, the model making each call in order and reading back each result.
    fn run(
        &self,
        containment: &str,
        extra: &str,
        calls: Vec<Call>,
        probe_b: bool,
        hook: bool,
    ) -> Run {
        let total = calls.len() + 1;
        let mut next = 0usize;
        let server = common::ScriptedServer::start_answering(total, move |request| {
            let index = next;
            next += 1;
            let Some(call) = calls.get(index) else {
                return end_turn();
            };
            let mut text = call.input.to_string();
            while let Some(at) = text.find("$HANDLE:") {
                let rest = &text[at + "$HANDLE:".len()..];
                let id: String = rest.chars().take_while(|c| *c != '"').collect();
                let result = common::tool_result_text(std::slice::from_ref(request), &id)
                    .unwrap_or_else(|| panic!("no result for {id} yet"));
                let handle = serde_json::from_str::<Value>(&result).unwrap()["handle"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{id} returned no handle: {result}"))
                    .to_string();
                text = text.replacen(&format!("$HANDLE:{id}"), &handle, 1);
            }
            common::tool_use_response(call.id, call.tool, serde_json::from_str(&text).unwrap())
        });
        self.write_manifest(&server.endpoint, containment, extra, probe_b, hook);
        let output = self
            .mur()
            .args(["run", "--manifest", self.manifest().to_str().unwrap()])
            .args(["--task", "Run each call once.", "--verbose"])
            .write_stdin("")
            .timeout(std::time::Duration::from_secs(300))
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains("status:  ok"),
            "the run must finish ok\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let records = common::parse_records_from_stdout(&stdout(&output));
        let trace = common::read_whole_trace(&records.join("trace.jsonl"));
        Run {
            output,
            requests: server.requests(),
            trace,
        }
    }
}

struct Run {
    output: Output,
    requests: Vec<Value>,
    trace: Vec<Value>,
}

impl Run {
    /// Call `id`'s result, parsed.
    #[allow(clippy::print_stdout)]
    fn result(&self, id: &str) -> Value {
        let text = common::tool_result_text(&self.requests, id)
            .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {}", stderr(&self.output)));
        println!("── {id}\n{}", text.chars().take(2000).collect::<String>());
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("{id} is not JSON: {text}"))
    }

    fn events(&self, event_type: &str) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn session_event_id(&self) -> String {
        self.events("session_start")[0]["event_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Every process line is the tool's, hangs off the session node and carries its fields.
    fn assert_process_lines_are_attributed(&self) {
        let session = self.session_event_id();
        for event_type in ["process_start", "process_exit", "process_refused"] {
            for event in self.events(event_type) {
                assert_eq!(event["parent_id"], session.as_str(), "{event}");
                assert!(
                    event["tool"] == PROBE || event["tool"] == PROBE_B,
                    "{event}"
                );
                if event_type != "process_refused" {
                    assert!(
                        event["process_id"].as_str().unwrap().starts_with("prc_"),
                        "{event}"
                    );
                    assert!(
                        event["binary"].as_str().unwrap().starts_with('/'),
                        "{event}"
                    );
                }
                if event_type == "process_start" {
                    assert!(event["argv"].is_array(), "{event}");
                    assert!(event["pid"].as_i64().unwrap() > 1, "{event}");
                }
            }
        }
        // One exit per start, after it.
        for start in self.events("process_start") {
            let id = &start["process_id"];
            let exits: Vec<_> = self
                .events("process_exit")
                .into_iter()
                .filter(|exit| &exit["process_id"] == id)
                .collect();
            assert_eq!(exits.len(), 1, "one process_exit for {start}");
            let position = |event: &Value| self.trace.iter().position(|e| e == event).unwrap();
            assert!(position(start) < position(exits[0]));
        }
    }
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn shell_allow(programs: &[&str]) -> String {
    let mut yaml = "  shell:\n    allow:\n".to_string();
    for program in programs {
        yaml.push_str(&format!("      - {program}\n"));
    }
    yaml
}

fn pid_exists(pid: i64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// No process of the group `pgid` is left.
fn group_is_empty(pgid: i64) -> bool {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .all(|entry| {
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                return true;
            };
            let Some(close) = stat.rfind(')') else {
                return true;
            };
            stat[close + 1..].split_whitespace().nth(2) != Some(pgid.to_string().as_str())
        })
}

fn refusal_rule(result: &Value) -> &str {
    assert_eq!(result["error"]["kind"], "refused", "{result}");
    result["error"]["rule"].as_str().unwrap()
}

/// `run`: a program runs to completion under the shell's rules, its output is capped, its time
/// limit kills it, a program outside `shell.allow` is refused, and each is traced.
#[test]
fn run_obeys_the_shell_rules() {
    if common::skip_without_host_support("run_obeys_the_shell_rules") {
        return;
    }
    let fixture = Fixture::new();
    for tier in tiers("run_obeys_the_shell_rules") {
        let run = fixture.run(
            tier,
            &shell_allow(&["cat", "sleep"]),
            vec![
                call("hello", json!({"op": "run", "program": "cat", "stdin": "hello\n"})),
                call(
                    "big",
                    json!({"op": "run", "program": "cat", "stdin_repeat": 1_572_864, "quiet": true}),
                ),
                call(
                    "slow",
                    json!({"op": "run", "program": "sleep", "args": ["30"], "timeout_ms": 500}),
                ),
                call("ls", json!({"op": "run", "program": "ls"})),
                call("start-ls", json!({"op": "start", "name": "ls", "program": "ls"})),
            ],
            false,
            false,
        );
        let hello = run.result("hello");
        assert_eq!(hello["exit_code"], 0, "{tier}");
        assert_eq!(hello["stdout"], "hello\n", "{tier}");
        assert_eq!(hello["stderr"], "", "{tier}");
        assert_eq!(hello["stdout_truncated"], false, "{tier}");
        assert_eq!(hello["stderr_truncated"], false, "{tier}");
        let big = run.result("big");
        assert_eq!(big["stdout_len"], 1_048_576, "{tier}");
        assert_eq!(big["stdout_truncated"], true, "{tier}");
        let slow = run.result("slow");
        assert_eq!(slow["timed_out"], true, "{tier}");
        assert_eq!(slow["exit_code"], 137, "{tier}");
        for label in ["ls", "start-ls"] {
            let refused = run.result(label);
            assert_eq!(refusal_rule(&refused), "capabilities.shell.allow", "{tier}");
            let reason = refused["error"]["reason"].as_str().unwrap_or_default();
            assert!(reason.contains("'ls'"), "{tier}: {label}: {reason}");
        }

        let refused = run.events("process_refused");
        assert_eq!(refused.len(), 2, "{tier}: {refused:?}");
        assert!(refused
            .iter()
            .all(|event| event["rule"] == "capabilities.shell.allow"));
        assert_eq!(refused[0]["kind"], "run");
        assert_eq!(refused[1]["kind"], "session");
        let starts = run.events("process_start");
        assert_eq!(starts.len(), 3, "{tier}: one start per run, none for ls");
        assert!(starts
            .iter()
            .all(|start| start["kind"] == "run" && start["name"].is_null()));
        let exits = run.events("process_exit");
        assert_eq!(exits[2]["reason"], "timed_out");
        assert_eq!(exits[0]["reason"], "exited");
        let sleep_pid = starts[2]["pid"].as_i64().unwrap();
        assert!(
            !pid_exists(sleep_pid),
            "{tier}: the timed-out sleep is gone"
        );
        assert!(
            group_is_empty(sleep_pid),
            "{tier}: the timed-out sleep's group is empty"
        );
        run.assert_process_lines_are_attributed();
    }
}

/// An `on-shell` deny hook refuses `run` and `start` on the argv they would run.
#[test]
fn an_on_shell_deny_hook_refuses_run_and_start() {
    if common::skip_without_host_support("an_on_shell_deny_hook_refuses_run_and_start") {
        return;
    }
    let fixture = Fixture::new();
    for tier in tiers("an_on_shell_deny_hook_refuses_run_and_start") {
        let run = fixture.run(
            tier,
            &shell_allow(&["cat"]),
            vec![
                call("run", json!({"op": "run", "program": "cat", "stdin": "x"})),
                call(
                    "start",
                    json!({"op": "start", "name": "c", "program": "cat"}),
                ),
            ],
            false,
            true,
        );
        for id in ["run", "start"] {
            let result = run.result(id);
            assert_eq!(refusal_rule(&result), "on-shell", "{tier}");
            let reason = result["error"]["reason"].as_str().unwrap();
            assert!(
                reason.contains(HOOK) && reason.contains("no programs today"),
                "{reason}"
            );
        }
        assert_eq!(run.events("call_denied").len(), 2, "{tier}");
        let refused = run.events("process_refused");
        assert_eq!(refused.len(), 2, "{tier}");
        assert!(refused.iter().all(|event| event["rule"] == "on-shell"));
        assert!(
            run.events("process_start").is_empty(),
            "{tier}: nothing started"
        );
    }
}

/// `start`: a session process is found, written to and read on later calls, is not started twice
/// under one name, and dies with the session.
#[test]
fn a_session_process_lives_across_calls_and_dies_with_the_session() {
    let name = "a_session_process_lives_across_calls_and_dies_with_the_session";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    for tier in tiers(name) {
        let run = fixture.run(
            tier,
            &shell_allow(&["cat"]),
            vec![
                call("start", json!({"op": "start", "name": "echo", "program": "cat"})),
                call(
                    "use",
                    json!({"op": "seq", "steps": [
                        {"op": "find", "name": "echo"},
                        {"op": "status", "name": "echo"},
                        {"op": "write", "name": "echo", "data": "ping\n"},
                        {"op": "wait_output", "name": "echo", "contains": "ping", "timeout_ms": 10000},
                    ]}),
                ),
                call("again", json!({"op": "start", "name": "echo", "program": "cat"})),
                call("keep", json!({"op": "start", "name": "keep", "program": "cat"})),
            ],
            false,
            false,
        );
        let started = run.result("start");
        assert_eq!(started["already_running"], false, "{tier}");
        let used = run.result("use");
        assert_eq!(used[0]["handle"], started["handle"], "{tier}");
        assert_eq!(used[1]["state"], "running", "{tier}");
        assert_eq!(used[3]["found"], true, "{tier}: {used}");
        let again = run.result("again");
        assert_eq!(again["handle"], started["handle"], "{tier}");
        assert_eq!(again["already_running"], true, "{tier}");

        let starts = run.events("process_start");
        assert_eq!(
            starts
                .iter()
                .filter(|start| start["name"] == "echo")
                .count(),
            1,
            "{tier}"
        );
        // `keep` is gone with its group, and its exit precedes `session_end`.
        let keep = starts.iter().find(|start| start["name"] == "keep").unwrap();
        let pid = keep["pid"].as_i64().unwrap();
        assert!(!pid_exists(pid), "{tier}: pid {pid} outlived the session");
        assert!(group_is_empty(pid), "{tier}: group {pid} is not empty");
        let exit_at = run
            .trace
            .iter()
            .position(|event| {
                event["event_type"] == "process_exit" && event["process_id"] == keep["process_id"]
            })
            .unwrap();
        assert_eq!(run.trace[exit_at]["reason"], "session_end", "{tier}");
        let end_at = run
            .trace
            .iter()
            .position(|event| event["event_type"] == "session_end")
            .unwrap();
        assert!(
            exit_at < end_at,
            "{tier}: process_exit precedes session_end"
        );
        run.assert_process_lines_are_attributed();
    }
}

/// `connect` reaches a port the handle's own process listens on and nothing else.
#[test]
fn connect_reaches_only_the_handles_own_listener() {
    let name = "connect_reaches_only_the_handles_own_listener";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    let (p, q, r): (u16, u16, u16) = (18741, 18742, 18743);
    let server = |port: u16| json!(["-m", "http.server", "--bind", "127.0.0.1", port.to_string()]);
    for tier in tiers(name) {
        // A second `network.allow` entry naming one host on port 443, so the session's egress
        // proxy holds a listener there that `connect` must not reach.
        let extra = format!("      - https://example.com\n{}", shell_allow(&["python3"]));
        let run = fixture.run(
            tier,
            &extra,
            vec![
                call(
                    "start",
                    json!({"op": "seq", "steps": [
                        {"op": "start", "name": "web", "program": "python3", "args": server(p),
                         "env": {"PYTHONUNBUFFERED": "1"}},
                        {"op": "start", "name": "other", "program": "python3", "args": server(q),
                         "env": {"PYTHONUNBUFFERED": "1"}},
                    ]}),
                ),
                call(
                    "wait",
                    json!({"op": "seq", "steps": [
                        {"op": "wait_output", "name": "web", "contains": "Serving HTTP", "timeout_ms": 20000},
                        {"op": "wait_output", "name": "other", "contains": "Serving HTTP", "timeout_ms": 20000},
                    ]}),
                ),
                call(
                    "fetch",
                    json!({"op": "connect", "name": "web", "port": p,
                           "send": "GET / HTTP/1.0\r\n\r\n", "timeout_ms": 5000}),
                ),
                call(
                    "refused",
                    json!({"op": "seq", "steps": [
                        {"op": "connect", "name": "web", "port": q, "send": ""},
                        {"op": "connect", "name": "web", "port": 443, "send": ""},
                        {"op": "connect", "name": "web", "port": r, "send": ""},
                    ]}),
                ),
            ],
            false,
            false,
        );
        let wait = run.result("wait");
        assert_eq!(wait[0]["found"], true, "{tier}: {wait}");
        assert_eq!(wait[1]["found"], true, "{tier}: {wait}");
        let fetched = run.result("fetch");
        assert!(
            fetched["response"]
                .as_str()
                .unwrap()
                .starts_with("HTTP/1.0 200"),
            "{tier}: {fetched}"
        );
        let refused = run.result("refused");
        for step in refused.as_array().unwrap() {
            assert_eq!(refusal_rule(step), "connect", "{tier}");
        }
        let traced: Vec<_> = run
            .events("process_refused")
            .into_iter()
            .filter(|event| event["kind"] == "connect")
            .collect();
        assert_eq!(traced.len(), 3, "{tier}");
        let ports: Vec<_> = traced
            .iter()
            .map(|event| event["port"].as_u64().unwrap())
            .collect();
        assert_eq!(ports, [u64::from(q), 443, u64::from(r)]);
        assert!(traced.iter().all(|event| event.get("binary").is_none()));
        run.assert_process_lines_are_attributed();
    }
}

/// Eight session processes start, the ninth is refused, and after one `stop` it starts.
#[test]
fn a_session_holds_at_most_eight_processes() {
    let name = "a_session_holds_at_most_eight_processes";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    for tier in tiers(name) {
        let starts: Vec<Value> = (1..=9)
            .map(|index| json!({"op": "start", "name": format!("cat-{index}"), "program": "cat"}))
            .collect();
        let run = fixture.run(
            tier,
            &shell_allow(&["cat"]),
            vec![
                call("nine", json!({"op": "seq", "steps": starts})),
                call(
                    "stop-then-start",
                    json!({"op": "seq", "steps": [
                        {"op": "stop", "name": "cat-1"},
                        {"op": "start", "name": "cat-9", "program": "cat"},
                    ]}),
                ),
            ],
            false,
            false,
        );
        let nine = run.result("nine");
        for step in &nine.as_array().unwrap()[..8] {
            assert!(
                step["handle"].as_str().unwrap().starts_with("prc_"),
                "{tier}: {step}"
            );
        }
        let ninth = &nine[8];
        assert_eq!(refusal_rule(ninth), "session-process-cap", "{tier}");
        assert!(ninth["error"]["reason"].as_str().unwrap().contains('8'));
        let then = run.result("stop-then-start");
        assert_eq!(then[0]["state"], "exited", "{tier}: {then}");
        assert_eq!(then[1]["already_running"], false, "{tier}: {then}");
        let capped: Vec<_> = run
            .events("process_refused")
            .into_iter()
            .filter(|event| event["rule"] == "session-process-cap")
            .collect();
        assert_eq!(capped.len(), 1, "{tier}");
        let stopped: Vec<_> = run
            .events("process_exit")
            .into_iter()
            .filter(|event| event["reason"] == "stopped")
            .collect();
        assert_eq!(stopped.len(), 1, "{tier}");
        assert_eq!(stopped[0]["name"], "cat-1");
        run.assert_process_lines_are_attributed();
    }
}

/// How many processes run `sleep <arg>`, read from the host's `/proc` (a zombie has no command
/// line, so the dead are not counted).
fn sleeps_running(arg: &str) -> usize {
    let wanted = format!("sleep\0{arg}\0");
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| cmdline == wanted.as_bytes())
        })
        .count()
}

/// A program that forks: when a session process's program exits, what it left in its process
/// group is killed, so an exited process holds nothing running and the cap counts what runs.
#[test]
fn a_session_process_that_exits_takes_its_group_with_it() {
    let name = "a_session_process_that_exits_takes_its_group_with_it";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    for (tier, sleep_arg) in tiers(name).into_iter().zip(["297.25", "297.75"]) {
        let program = format!("sleep {sleep_arg} & echo started $!");
        let steps: Vec<Value> = (1..=9)
            .flat_map(|index| {
                let name = format!("fork-{index}");
                [
                    json!({"op": "start", "name": name, "program": "sh", "args": ["-c", program]}),
                    json!({"op": "wait_output", "name": name, "contains": "started"}),
                    json!({"op": "wait_exit", "name": name}),
                ]
            })
            .collect();

        // A host-side watch over the whole run: the most `sleep`s of this scenario alive at once.
        let watching = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let watcher = {
            let watching = std::sync::Arc::clone(&watching);
            let sleep_arg = sleep_arg.to_string();
            std::thread::spawn(move || {
                let mut most = 0;
                while watching.load(std::sync::atomic::Ordering::SeqCst) {
                    most = most.max(sleeps_running(&sleep_arg));
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                most
            })
        };
        let run = fixture.run(
            tier,
            &shell_allow(&["sh", "sleep"]),
            vec![call("forks", json!({"op": "seq", "steps": steps}))],
            false,
            false,
        );
        watching.store(false, std::sync::atomic::Ordering::SeqCst);
        let most = watcher.join().unwrap();

        let forks = run.result("forks");
        for step in forks.as_array().unwrap().chunks(3) {
            let label = format!("{tier}: {step:?}");
            assert_eq!(step[0]["already_running"], false, "{label}");
            assert_eq!(step[1]["found"], true, "{label}");
            assert_eq!(step[2]["state"], "exited", "{label}");
        }
        // Nine programs each forked a `sleep`; each `sleep` died with its program, before the next
        // started. Without that, nine would run at once.
        assert!(
            most <= 2,
            "{tier}: {most} sleeps of exited programs ran at once"
        );
        assert!(run.events("process_refused").is_empty(), "{tier}");
        let starts = run.events("process_start");
        assert_eq!(starts.len(), 9, "{tier}");
        for exit in run.events("process_exit") {
            assert_eq!(exit["reason"], "exited", "{tier}: {exit}");
        }
        for start in &starts {
            let pgid = start["pid"].as_i64().unwrap();
            assert!(group_is_empty(pgid), "{tier}: group {pgid} is not empty");
        }
        assert_eq!(sleeps_running(sleep_arg), 0, "{tier}");
        run.assert_process_lines_are_attributed();
    }
}

/// A second tool sees none of the first tool's processes and starts its own.
#[test]
fn another_tool_cannot_reach_the_first_tools_processes() {
    let name = "another_tool_cannot_reach_the_first_tools_processes";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    for tier in tiers(name) {
        let run = fixture.run(
            tier,
            &shell_allow(&["cat"]),
            vec![
                call(
                    "start",
                    json!({"op": "start", "name": "echo", "program": "cat"}),
                ),
                Call {
                    id: "other",
                    tool: PROBE_B,
                    input: json!({"op": "seq", "steps": [
                        {"op": "find", "name": "echo"},
                        {"op": "status", "handle": "$HANDLE:start"},
                        {"op": "start", "name": "echo", "program": "cat"},
                    ]}),
                },
            ],
            true,
            false,
        );
        let started = run.result("start");
        let other = run.result("other");
        assert!(other[0]["handle"].is_null(), "{tier}: {other}");
        assert_eq!(
            other[1]["error"]["kind"], "unknown-handle",
            "{tier}: {other}"
        );
        assert_eq!(other[2]["already_running"], false, "{tier}: {other}");
        assert_ne!(other[2]["handle"], started["handle"], "{tier}");
        let tools: Vec<_> = run
            .events("process_start")
            .iter()
            .map(|start| start["tool"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(tools, [PROBE, PROBE_B], "{tier}");
        run.assert_process_lines_are_attributed();
    }
}
