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
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const PROBE: &str = "process-probe";
const PROBE_B: &str = "process-probe-b";
const VERSION: &str = "0.1.0";
const HOOK: &str = "no-programs";

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

    /// The capsule at `containment`, or declaring none when it is empty, with `extra` lines under
    /// `capabilities:`, the second probe when `probe_b`, and the deny hook when `hook`.
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
        let containment = if containment.is_empty() {
            String::new()
        } else {
            format!("  containment: {containment}\n")
        };
        fs::write(
            self.manifest(),
            format!(
                "name: session-processes-capsule\nversion: 0.0.1\nartifacts:\n{artifacts}\
                 capabilities:\n{containment}  network:\n    allow:\n      - \
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
        self.run_with_args(containment, extra, calls, probe_b, hook, &[])
    }

    /// [`Fixture::run`], with `args` added to `mur run`.
    fn run_with_args(
        &self,
        containment: &str,
        extra: &str,
        calls: Vec<Call>,
        probe_b: bool,
        hook: bool,
        args: &[&str],
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
            .args(args)
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
    for tier in common::tiers("run_obeys_the_shell_rules") {
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
    for tier in common::tiers("an_on_shell_deny_hook_refuses_run_and_start") {
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
    for tier in common::tiers(name) {
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
    for tier in common::tiers(name) {
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
    for tier in common::tiers(name) {
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
    for (tier, sleep_arg) in common::tiers(name).into_iter().zip(["297.25", "297.75"]) {
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
    for tier in common::tiers(name) {
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

// ---------------------------------------------------------------- one containment, two routes

/// One capsule shape the routes are compared at: its label, the `containment:` it declares (none
/// when empty) and its `capabilities:` lines.
struct Shape {
    label: &'static str,
    containment: &'static str,
    extra: String,
}

/// What the fingerprint script needs to exec, allowed identically for every route.
const FINGERPRINT_ALLOW: &[&str] = &["bash", "readlink", "rm"];

/// `scoped`, `sealed` when the host reaches it, and the shape of a capsule that declares no
/// containment and sets `filesystem.workdir_exec: true`, which runs `advisory` on the Full tier.
fn fingerprint_shapes(test_name: &str) -> Vec<Shape> {
    let mut shapes: Vec<Shape> = common::tiers(test_name)
        .into_iter()
        .map(|tier| Shape {
            label: tier,
            containment: tier,
            extra: shell_allow(FINGERPRINT_ALLOW),
        })
        .collect();
    shapes.push(Shape {
        label: "advisory+workdir_exec",
        containment: "",
        extra: format!(
            "{}  filesystem:\n    workdir_exec: true\n",
            shell_allow(FINGERPRINT_ALLOW)
        ),
    });
    shapes
}

/// The five ways one session runs the fingerprint script: a `bash` shell call and a
/// `murmur:process` `run` of the same `bash -c`, each at depth 1 and depth 2, and a `start`.
const ROUTES: &[&str] = &["shell-d1", "shell-d2", "run-d1", "run-d2", "start-d1"];

/// One fingerprint per route, keyed by field. Depth-2 keys keep their `d2.` prefix.
type Fingerprints = std::collections::BTreeMap<&'static str, Fingerprint>;
type Fingerprint = std::collections::BTreeMap<String, String>;

/// The key families the script prints, the first word of every key it prints.
const FINGERPRINT_FAMILIES: &[&str] = &[
    "status",
    "limit",
    "cgroup",
    "lsm",
    "oom_score_adj",
    "personality",
    "ns",
    "proc",
    "fd",
    "env",
    "dev",
    "devshm",
    "mount",
    "landlock",
    "end",
];

/// The `key: value` lines of the script's output; every other line (the shell result's
/// `$ <command>` head) is skipped.
fn parse_fingerprint(output: &str) -> Fingerprint {
    output
        .lines()
        .filter_map(|line| line.split_once(": "))
        .filter(|(key, _)| {
            let bare = key.strip_prefix("d2.").unwrap_or(key);
            let family = bare.split(['.', ':']).next().unwrap_or_default();
            FINGERPRINT_FAMILIES.contains(&family) && !key.contains(' ')
                || bare.starts_with("limit.")
        })
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Runs the fingerprint script every way in [`ROUTES`] in one session of `shape`.
fn fingerprint_routes(fixture: &Fixture, shape: &Shape) -> Fingerprints {
    let workdir = fixture.project.join(format!("work-{}", shape.label));
    let _ = fs::remove_dir_all(&workdir);
    fs::create_dir_all(&workdir).unwrap();
    fs::copy(
        common::fixture_path("containment-fingerprint/fingerprint.sh"),
        workdir.join("fingerprint.sh"),
    )
    .unwrap();
    let shell = |id: &'static str, command: &str| Call {
        id,
        tool: "bash",
        input: json!({"command": command}),
    };
    let run_bash = |id: &'static str, command: &str| {
        call(
            id,
            json!({"op": "run", "program": "bash", "args": ["-c", command], "timeout_ms": 120_000}),
        )
    };
    let run = fixture.run_with_args(
        shape.containment,
        &shape.extra,
        vec![
            shell("shell-d1", "bash fingerprint.sh"),
            shell("shell-d2", "bash fingerprint.sh --depth2"),
            run_bash("run-d1", "bash fingerprint.sh"),
            run_bash("run-d2", "bash fingerprint.sh --depth2"),
            call(
                "start-d1",
                json!({"op": "seq", "steps": [
                    {"op": "start", "name": "fingerprint", "program": "bash",
                     "args": ["-c", "bash fingerprint.sh"]},
                    {"op": "wait_output", "name": "fingerprint", "contains": "end: ok",
                     "timeout_ms": 120_000},
                ]}),
            ),
        ],
        false,
        false,
        &["--workdir", workdir.to_str().unwrap()],
    );
    let label = shape.label;
    let mut fingerprints = Fingerprints::new();
    for route in ["shell-d1", "shell-d2"] {
        let text = common::tool_result_text(&run.requests, route)
            .unwrap_or_else(|| panic!("{label}: no result for {route}"));
        fingerprints.insert(route, parse_fingerprint(&text));
    }
    for route in ["run-d1", "run-d2"] {
        let result = run.result(route);
        assert_eq!(result["exit_code"], 0, "{label}: {route}: {result}");
        fingerprints.insert(
            route,
            parse_fingerprint(result["stdout"].as_str().unwrap_or_default()),
        );
    }
    let started = run.result("start-d1");
    assert_eq!(started[1]["found"], true, "{label}: start-d1: {started}");
    fingerprints.insert(
        "start-d1",
        parse_fingerprint(started[1]["output"].as_str().unwrap_or_default()),
    );
    for probe in ["tmp", "dev/shm"] {
        for entry in fs::read_dir(format!("/{probe}"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("murmur-fingerprint.")
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    fingerprints
}

/// The fields compared by name: each must be present on both routes and equal.
const COMPARED_KEYS: &[&str] = &[
    "status.Uid",
    "status.Gid",
    "status.Groups",
    "status.CapInh",
    "status.CapPrm",
    "status.CapEff",
    "status.CapBnd",
    "status.CapAmb",
    "status.NoNewPrivs",
    "status.Seccomp",
    "status.Seccomp_filters",
    "status.SigBlk",
    "status.SigIgn",
    "status.SigCgt",
    "status.Umask",
    "status.Cpus_allowed_list",
    "status.NSpid.depth",
    "limit.Max cpu time",
    "limit.Max file size",
    "limit.Max data size",
    "limit.Max stack size",
    "limit.Max core file size",
    "limit.Max resident set",
    "limit.Max processes",
    "limit.Max open files",
    "limit.Max locked memory",
    "limit.Max address space",
    "limit.Max file locks",
    "limit.Max pending signals",
    "limit.Max msgqueue size",
    "limit.Max nice priority",
    "limit.Max realtime priority",
    "limit.Max realtime timeout",
    "cgroup",
    "lsm.current",
    "oom_score_adj",
    "personality",
    "ns.cgroup",
    "ns.ipc",
    "ns.net",
    "ns.pid",
    "ns.pid_for_children",
    "ns.time",
    "ns.time_for_children",
    "ns.user",
    "ns.uts",
    "proc.ppid",
    "proc.sid",
    "proc.pgid_is_pid",
    "proc.sid_is_pid",
    "proc.nice",
    "proc.pdeathsig",
    "dev.entries",
    "mount.count",
    "landlock.read_etc_passwd",
    "landlock.list_home",
    "landlock.write_workdir",
    "landlock.write_tmp",
    "landlock.write_dev_shm",
    "landlock.exec_unallowed",
    "end",
];

/// The families compared key by key: every key of the family on either route must be on both,
/// and equal.
const COMPARED_FAMILIES: &[&str] = &["env.", "fd.", "mount./"];

/// The fields that may differ at the shape labelled `shape`, each with the rule that allows it. A
/// key is tolerated only on the comparisons and shapes named here; on every other it is compared
/// like the rest.
///
/// No `env.` key is tolerated: the tool's own `env` additions are the one environment difference
/// the containment reference allows, and these calls pass none.
fn tolerated(shape: &str, key: &str, pair: (&str, &str)) -> Option<&'static str> {
    let bare = key.strip_prefix("d2.").unwrap_or(key);
    match bare {
        "proc.pid" => Some("a pid value: every process has its own"),
        "proc.ppid" if key.starts_with("d2.") => {
            Some("a pid value: a depth-2 process's parent is its depth-1 process")
        }
        "ns.mnt" => Some(
            "the per-command mount namespace: every command of a contained session gets its own",
        ),
        "ns.user" if shape == "sealed" => Some(
            "the per-command user namespace: every command of a sealed session gets its own, \
             nested in the session's",
        ),
        "fd.0" if pair.1 == "start-d1" => Some(
            "stdin: `/dev/null` for a shell command and a `run` without stdin, a pipe for `start`",
        ),
        _ => None,
    }
}

/// The value a key is compared on. `devshm.entries` is compared on whether `/dev/shm` could be
/// listed, or the refusal: the host's `/dev/shm` is shared with every process on it, so what it
/// holds changes between two calls, and what `/dev/shm` is, is compared by `mount./dev/shm`.
fn compared_value<'a>(key: &str, value: &'a str) -> &'a str {
    let bare = key.strip_prefix("d2.").unwrap_or(key);
    if bare == "devshm.entries" && !value.starts_with("unreadable:") {
        return "<listed>";
    }
    value
}

/// A field the script cannot read from a shell on any host, which it prints as such.
fn not_readable(value: &str) -> bool {
    value.starts_with("not readable")
}

/// Every difference between route `a` and route `b` at the shape labelled `shape` outside
/// [`tolerated`], one line each.
fn route_differences(
    shape: &str,
    fingerprints: &Fingerprints,
    a: &str,
    b: &str,
    depth2: bool,
) -> Vec<String> {
    let prefix = if depth2 { "d2." } else { "" };
    let left = &fingerprints[a];
    let right = &fingerprints[b];
    let mut differences = Vec::new();
    for (route, fingerprint) in [(a, left), (b, right)] {
        let seccomp = fingerprint.get(&format!("{prefix}status.Seccomp"));
        assert!(
            seccomp.is_some_and(|value| !value.starts_with("unreadable")),
            "{route}: no readable `{prefix}status.Seccomp` line, so nothing is compared: \
             {fingerprint:#?}"
        );
        let limits = fingerprint
            .keys()
            .filter(|key| key.starts_with(&format!("{prefix}limit.Max ")))
            .count();
        assert!(
            limits >= 16,
            "{route}: no limits table at {prefix}, so nothing is compared: {fingerprint:#?}"
        );
    }
    let mut keys: std::collections::BTreeSet<String> = COMPARED_KEYS
        .iter()
        .map(|key| format!("{prefix}{key}"))
        .collect();
    keys.insert(format!("{prefix}proc.pid"));
    keys.insert(format!("{prefix}ns.mnt"));
    keys.insert(format!("{prefix}devshm.entries"));
    for fingerprint in [left, right] {
        for key in fingerprint.keys() {
            let Some(bare) = key.strip_prefix(prefix) else {
                continue;
            };
            if prefix.is_empty() && key.starts_with("d2.") {
                continue;
            }
            if COMPARED_FAMILIES
                .iter()
                .any(|family| bare.starts_with(family))
            {
                keys.insert(key.clone());
            } else if !keys.contains(key) {
                differences.push(format!(
                    "{key}: printed by the script but neither compared nor tolerated"
                ));
            }
        }
    }
    for key in keys {
        if tolerated(shape, &key, (a, b)).is_some() {
            continue;
        }
        match (left.get(&key), right.get(&key)) {
            // Present on both routes and unreadable on both: the record says so, and two missing
            // values are never counted as equal.
            (Some(l), Some(r)) if not_readable(l) && not_readable(r) => {}
            (Some(l), Some(r)) if compared_value(&key, l) == compared_value(&key, r) => {}
            (Some(l), Some(r)) => differences.push(format!("{key}: {a} `{l}`, {b} `{r}`")),
            (l, r) => differences.push(format!(
                "{key}: on one route only ({a} {:?}, {b} {:?})",
                l.map(|_| "present"),
                r.map(|_| "present")
            )),
        }
    }
    differences
}

/// The record's markdown table for one shape: a row per field, a column per route, and whether
/// the routes agree.
fn fingerprint_table(label: &str, fingerprints: &Fingerprints) -> String {
    fn cell(value: Option<&String>) -> String {
        let Some(value) = value else {
            return "—".to_string();
        };
        let value = value.replace('|', "\\|");
        if value.chars().count() <= 48 {
            return format!("`{value}`");
        }
        // A long value prints its head and a digest, so two cells agree exactly when their
        // digests do.
        let digest = value.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        let head: String = value.chars().take(32).collect();
        format!(
            "`{head}…` ({} chars, fnv {digest:016x})",
            value.chars().count()
        )
    }
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for fingerprint in fingerprints.values() {
        for key in fingerprint.keys() {
            keys.insert(key.strip_prefix("d2.").unwrap_or(key).to_string());
        }
    }
    let mut table = format!(
        "#### {label}\n\n| field | shell d1 | shell d2 | process run d1 | process run d2 | \
         process start d1 | equal |\n|---|---|---|---|---|---|---|\n"
    );
    for key in keys {
        let d2 = format!("d2.{key}");
        let shell_d1 = fingerprints["shell-d1"].get(&key);
        let shell_d2 = fingerprints["shell-d2"].get(&d2);
        let run_d1 = fingerprints["run-d1"].get(&key);
        let run_d2 = fingerprints["run-d2"].get(&d2);
        let start_d1 = fingerprints["start-d1"].get(&key);
        let same = |left: Option<&String>, right: Option<&String>, k: &str| match (left, right) {
            (Some(l), Some(r)) => compared_value(k, l) == compared_value(k, r),
            (None, None) => true,
            _ => false,
        };
        let pairs = [
            (
                same(shell_d1, run_d1, &key),
                tolerated(label, &key, ("shell-d1", "run-d1")),
            ),
            (
                same(shell_d2, run_d2, &d2),
                tolerated(label, &d2, ("shell-d2", "run-d2")),
            ),
            (
                same(shell_d1, start_d1, &key),
                tolerated(label, &key, ("shell-d1", "start-d1")),
            ),
        ];
        let equal = if [shell_d1, shell_d2, run_d1, run_d2, start_d1]
            .iter()
            .all(|value| value.is_some_and(|value| not_readable(value)))
        {
            "not compared: not readable from a shell".to_string()
        } else if pairs.iter().all(|(same, _)| *same) {
            "yes".to_string()
        } else if let Some(rule) = pairs
            .iter()
            .filter(|(same, _)| !same)
            .map(|(_, rule)| *rule)
            .collect::<Option<Vec<_>>>()
        {
            format!("no, allowed: {}", rule[0])
        } else {
            "**no**".to_string()
        };
        table.push_str(&format!(
            "| `{key}` | {} | {} | {} | {} | {} | {equal} |\n",
            cell(shell_d1),
            cell(shell_d2),
            cell(run_d1),
            cell(run_d2),
            cell(start_d1)
        ));
    }
    table
}

/// A program started from a `bash` shell call and the same program a WASM tool runs through
/// `murmur:process` run under the same containment, field by field, at depth 1 and at depth 2:
/// seccomp, `no_new_privs`, capabilities, rlimits, cgroup, namespaces, environment, open
/// descriptors, signal mask and dispositions, process group, `/dev` and `/dev/shm`, and the
/// Landlock outcomes. A `start` is compared with the shell call at depth 1 too.
#[test]
#[allow(clippy::print_stdout)]
fn a_shell_command_and_a_session_process_are_contained_alike() {
    let name = "a_shell_command_and_a_session_process_are_contained_alike";
    if common::skip_without_host_support(name) {
        return;
    }
    let fixture = Fixture::new();
    let mut failures = Vec::new();
    for shape in fingerprint_shapes(name) {
        let fingerprints = fingerprint_routes(&fixture, &shape);
        assert_eq!(fingerprints.len(), ROUTES.len());
        println!("{}", fingerprint_table(shape.label, &fingerprints));
        for (a, b, depth2) in [
            ("shell-d1", "run-d1", false),
            ("shell-d2", "run-d2", true),
            ("shell-d1", "start-d1", false),
        ] {
            for difference in route_differences(shape.label, &fingerprints, a, b, depth2) {
                failures.push(format!("{}: {difference}", shape.label));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the routes are contained differently:\n{}",
        failures.join("\n")
    );
}

/// What `sh -c 'seq 1 20000; seq 1 3000 >&2'` prints on stdout and on stderr.
fn known_block() -> (String, String) {
    let lines = |last: u32| (1..=last).map(|n| format!("{n}\n")).collect::<String>();
    (lines(20000), lines(3000))
}

/// A tool that polls `status` until `exited` and then reads once gets every byte the program
/// wrote, with both streams ended, and a second read gets nothing more, on every tier.
#[test]
fn a_tool_that_waits_for_the_exit_reads_every_byte() {
    let name = "a_tool_that_waits_for_the_exit_reads_every_byte";
    if common::skip_without_host_support(name) {
        return;
    }
    const CALLS: usize = 4;
    const RUNS_PER_CALL: usize = 50;
    let (stdout, stderr) = known_block();
    let fixture = Fixture::new();
    for tier in common::tiers(name) {
        let calls = (0..CALLS)
            .map(|index| {
                let id: &'static str = ["drain-0", "drain-1", "drain-2", "drain-3"][index];
                call(
                    id,
                    json!({
                        "op": "drain",
                        "program": "sh",
                        "args": ["-c", "seq 1 20000; seq 1 3000 >&2"],
                        "runs": RUNS_PER_CALL,
                        "prefix": id,
                        "expect_stdout_len": stdout.len(),
                        "expect_stderr_len": stderr.len(),
                        "timeout_ms": 20_000,
                    }),
                )
            })
            .collect();
        let extra = format!(
            "{}  limits:\n    deadline_seconds: 240\n",
            shell_allow(&["sh", "seq"])
        );
        let run = fixture.run(tier, &extra, calls, false, false);
        let mut total = 0;
        for index in 0..CALLS {
            let result = run.result(&format!("drain-{index}"));
            assert_eq!(
                result["misses"],
                json!([]),
                "{tier}: call {index}: {result}"
            );
            total += result["runs"].as_u64().unwrap();
            assert!(
                result["first_stdout_tail"]
                    .as_str()
                    .unwrap()
                    .ends_with("19999\n20000\n"),
                "{tier}: call {index}: {result}"
            );
        }
        assert!(total >= 200, "{tier}: {total} runs");
        assert!(run.events("process_refused").is_empty(), "{tier}");
    }
}

/// A tool built against `murmur:process@0.1.0`, WIT level 6, is refused at staging: level 7 has
/// `murmur:process@0.2.0`, and the host serves no other version.
#[test]
fn a_process_tool_built_for_level_6_is_refused() {
    let fixture = Fixture::new();
    let old = common::create_tool_artifact(
        &fixture.artifacts,
        "local/process-probe-v1",
        VERSION,
        &common::fixture_path("process-probe-v1/tool/process-probe-v1.wasm"),
    );
    let driver = common::create_driver_artifact(
        &fixture.artifacts,
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    for artifact in [&driver, &old] {
        fixture
            .mur()
            .args(["publish", artifact.to_str().unwrap()])
            .assert()
            .success();
    }
    // A project of its own, with no `murmur.lock`, resolving from the global store: the fixture's
    // project locks the current probe.
    fixture.write_manifest("http://127.0.0.1:9", "", "", false, false);
    let manifest = fs::read_to_string(fixture.manifest())
        .unwrap()
        .replace(&format!("local/{PROBE}\n"), "local/process-probe-v1\n");
    let project = fixture.project.with_file_name("old-level-project");
    fs::create_dir_all(&project).unwrap();
    let manifest_path = project.join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    let output = fixture
        .mur()
        .current_dir(&project)
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .args(["--task", "hi"])
        .write_stdin("")
        .timeout(std::time::Duration::from_secs(120))
        .output()
        .unwrap();
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("error[E-WIT-LEVEL-001]"), "{text}");
    assert!(
        text.contains(&format!(
            "artifact 'local/process-probe-v1@{VERSION}' does not fit WIT level 7, the level this \
             mur serves: it imports murmur:process/process@0.1.0, and level 7 has \
             murmur:process@0.2.0"
        )),
        "{text}"
    );
}
