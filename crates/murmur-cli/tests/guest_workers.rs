//! A `mur run` session runs its WASM tools, drivers, hooks and script component in guest workers:
//! `mur __guest-worker <kind>` children of `mur`, each in its own process group, with an empty
//! environment, started only for the kinds the capsule has, gone when the session ends. A worker
//! killed mid-task fails the task with `E-RUN-065` and the run's records still end with
//! `session_end`.
//!
//! Driven through the real `mur` binary against a loopback upstream the test owns.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const STALL_DRIVER: &str = "local/stall-driver";
const STALL_TOOL: &str = "local/stall-tool";
const ECHO_TOOL: &str = "local/echo-tool";
const SLEEP_HOOK: &str = "local/sleep-hook";
const PROCESS_DRIVER: &str = "local/fixture-process-driver";
const VERSION: &str = "0.1.0";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const KEY: &str = "sk-guest-worker-canary-41d9";
const WAIT: Duration = Duration::from_secs(120);

const END_TURN: &str = r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

fn mur() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// A scratch `HOME` with the drivers and tools published and precompiled.
struct Home {
    home: TempDir,
}

impl Home {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let mut published = Vec::new();
        for (name, wasm) in [
            (
                ANTHROPIC,
                "drivers/anthropic/driver/murmur-driver-anthropic.wasm",
            ),
            (STALL_DRIVER, "stall-driver/tool/stall-driver.wasm"),
        ] {
            published.push(common::create_driver_artifact_with_auth(
                artifacts.path(),
                name,
                VERSION,
                &common::fixture_path(wasm),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            ));
        }
        published.push(common::create_tool_artifact(
            artifacts.path(),
            STALL_TOOL,
            VERSION,
            &common::fixture_path("stall-driver/tool/stall-driver.wasm"),
        ));
        published.push(common::create_tool_artifact(
            artifacts.path(),
            ECHO_TOOL,
            VERSION,
            &common::fixture_path("run/components/echo-tool.wasm"),
        ));
        published.push(sleep_hook_zip(artifacts.path()));
        published.push(common::create_driver_artifact_with_auth(
            artifacts.path(),
            PROCESS_DRIVER,
            VERSION,
            &common::fixture_path("process-driver/tool/process-driver.wasm"),
            "",
        ));
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

    fn capsule(&self, driver: &str, endpoint: &str, extra: &str) -> Capsule {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: worker-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: {KEY}\n{extra}inference:\n  transport: http\n  \
                 model: test-model\n  max_turns: 4\n  driver:\n    artifact: {driver}\n"
            ),
        )
        .unwrap();
        Capsule { project, manifest }
    }

    fn spawn(&self, capsule: &Capsule) -> Child {
        Command::new(mur())
            .env("HOME", self.home.path())
            .env("MURMUR_TEST_CANARY", KEY)
            .current_dir(capsule.project.path())
            .args(["run", "--manifest", capsule.manifest.to_str().unwrap()])
            .args(["--task", "Say hello.", "--verbose"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn run(&self, capsule: &Capsule) -> Run {
        let child = self.spawn(capsule);
        finish(child, WAIT)
    }
}

fn finish(mut child: Child, wait: Duration) -> Run {
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > wait {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Run::of(child.wait_with_output().unwrap(), started.elapsed())
}

struct Capsule {
    project: TempDir,
    manifest: PathBuf,
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
    took: Duration,
}

impl Run {
    fn of(output: Output, took: Duration) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
            took,
        }
    }

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout)
    }

    fn trace(&self) -> Vec<Value> {
        let trace = self.records_dir().join("trace.jsonl");
        assert!(trace.is_file(), "no trace.jsonl; {}", self.context());
        common::read_whole_trace(&trace)
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn event_types(&self) -> Vec<String> {
        self.trace()
            .iter()
            .filter_map(|event| event["event_type"].as_str().map(str::to_string))
            .collect()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

/// A loopback provider answering each request with the next scripted body, recording each
/// request's head and body.
struct Upstream {
    endpoint: String,
    heads: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Upstream {
    fn start(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (thread_heads, thread_bodies) = (Arc::clone(&heads), Arc::clone(&bodies));
        thread::spawn(move || {
            let mut responses = responses.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let Ok(read) = stream.read(&mut chunk) else {
                        break (String::new(), Vec::new());
                    };
                    if read == 0 {
                        break (String::new(), Vec::new());
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        while buffer.len() < end + 4 + length {
                            let Ok(read) = stream.read(&mut chunk) else {
                                break;
                            };
                            if read == 0 {
                                break;
                            }
                            buffer.extend_from_slice(&chunk[..read]);
                        }
                        break (head, buffer[end + 4..].to_vec());
                    }
                };
                thread_heads.lock().unwrap().push(head);
                if let Ok(body) = serde_json::from_slice::<Value>(&body) {
                    thread_bodies.lock().unwrap().push(body);
                }
                let reply = responses.next().unwrap_or_else(|| END_TURN.to_string());
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        Self {
            endpoint,
            heads,
            bodies,
        }
    }
}

/// The `mur __guest-worker <kind>` children of `pid`: (pid, kind, argv, pgid).
fn workers_of(pid: u32) -> Vec<(u32, String, Vec<String>, i32)> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(child) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{child}/stat")) else {
            continue;
        };
        // Fields after the parenthesised command: state, ppid, pgrp.
        let after = &stat[stat.rfind(')').unwrap() + 2..];
        let fields: Vec<&str> = after.split(' ').collect();
        if fields[1].parse::<u32>().ok() != Some(pid) {
            continue;
        }
        let Ok(cmdline) = fs::read(format!("/proc/{child}/cmdline")) else {
            continue;
        };
        let argv: Vec<String> = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        if argv.get(1).map(String::as_str) == Some("__guest-worker") {
            let kind = argv.get(2).cloned().unwrap_or_default();
            found.push((child, kind, argv, fields[2].parse().unwrap()));
        }
    }
    found.sort_by(|a, b| a.1.cmp(&b.1));
    found
}

fn pgid_of(pid: u32) -> i32 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split(' ').nth(2).unwrap().parse().unwrap()
}

/// Waits until `pid` has at least `count` worker children, or `wait` passes.
fn wait_for_workers(
    pid: u32,
    count: usize,
    wait: Duration,
) -> Vec<(u32, String, Vec<String>, i32)> {
    let started = Instant::now();
    loop {
        let found = workers_of(pid);
        if found.len() >= count || started.elapsed() > wait {
            return found;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Holds a `mur run --task` session open once its task has ended: under `task_acceptance: queue`
/// the session waits at its door for the next task until it is signalled.
fn sleep_lifecycle() -> &'static str {
    "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n"
}

/// The sleeping hook, packed with its bundled manifest: `execution_mode: blocking` and no
/// `binding:`, so it sleeps through `on-session-start` until its entry's deadline.
fn sleep_hook_zip(dir: &Path) -> PathBuf {
    use zip::{
        write::{FileOptions, SimpleFileOptions},
        CompressionMethod, ZipWriter,
    };
    let path = dir.join(format!("sleep-hook-{VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("sleep-hook/murmur.yaml")).unwrap())
        .unwrap();
    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("sleep-hook/hook/sleep-hook.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    path
}

/// A `runtime: hook` entry for the sleeping hook, stopped at `deadline` seconds.
fn sleep_hook_entry(deadline: u64) -> String {
    format!(
        "  - name: {SLEEP_HOOK}\n    version: {VERSION}\n    runtime: hook\n    capabilities:\n      \
         limits:\n        deadline_seconds: {deadline}\n"
    )
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| !stat.contains(") Z "))
            .unwrap_or(false)
}

/// The value, its hex, both base64 alphabets and its sha256.
fn leak_forms(secret: &str) -> Vec<String> {
    use base64::Engine as _;
    use sha2::Digest as _;
    vec![
        secret.to_string(),
        secret.bytes().map(|b| format!("{b:02x}")).collect(),
        base64::engine::general_purpose::STANDARD.encode(secret),
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret),
        sha2::Sha256::digest(secret.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ]
}

/// A task with a WASM tool completes `ok` through the driver and tools workers: the tool's result
/// reaches the model fenced, and the driver's request reached the provider with the key the
/// supervisor attached.
#[test]
fn a_task_runs_through_the_driver_and_tools_workers() {
    let home = Home::new();
    let upstream = Upstream::start(vec![
        common::tool_use_response("toolu_echo", "echo-tool", json!({"text": "ping"})),
        END_TURN.to_string(),
    ]);
    let tool = format!("  - name: {ECHO_TOOL}\n    version: {VERSION}\n    runtime: tool\n");
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &tool);
    let run = home.run(&capsule);

    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let types = run.event_types();
    let expected = [
        "session_start",
        "task_start",
        "inference",
        "tool_call",
        "inference",
        "task_end",
        "session_end",
    ];
    let mut at = 0;
    for event in &types {
        if at < expected.len() && event == expected[at] {
            at += 1;
        }
    }
    assert_eq!(at, expected.len(), "trace order {types:?}");
    assert_eq!(run.of_type("task_end")[0]["exit_status"], "ok");
    let bodies = upstream.bodies.lock().unwrap().clone();
    let result = common::find_tool_result(&bodies, "toolu_echo")
        .map(|block| common::extract_result_text(&block))
        .unwrap_or_else(|| panic!("no tool result fed back: {bodies:#?}"));
    assert!(
        result.contains("<untrusted-content") && result.contains("tool:echo-tool"),
        "the tool result reached the model fenced: {result}"
    );
    let heads = upstream.heads.lock().unwrap().clone();
    assert!(heads.len() >= 2, "{heads:#?}");
    for head in &heads {
        assert!(
            head.lines()
                .any(|line| line.eq_ignore_ascii_case(&format!("x-api-key: {KEY}"))),
            "the supervisor attached the key: {head}"
        );
    }
}

/// With the session held open at its door, the capsule's workers are children of `mur`, each in its own
/// process group, with an argv of exactly `<mur> __guest-worker <kind>`, an unreadable environment
/// and no form of the key on its command line; a capsule with a driver and a WASM tool runs two,
/// and none survives the session.
#[test]
fn workers_are_children_of_mur_and_end_with_the_session() {
    let home = Home::new();
    let upstream = Upstream::start(vec![END_TURN.to_string()]);
    let tool = format!(
        "  - name: {ECHO_TOOL}\n    version: {VERSION}\n    runtime: tool\n{}",
        sleep_lifecycle()
    );
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &tool);
    let child = home.spawn(&capsule);
    let pid = child.id();
    let workers = wait_for_workers(pid, 2, Duration::from_secs(60));
    // Give a lazily started worker, if any, the chance to show up.
    thread::sleep(Duration::from_millis(500));
    let workers_now = workers_of(pid);
    let kinds: Vec<&str> = workers_now.iter().map(|w| w.1.as_str()).collect();
    let mur_pgid = pgid_of(pid);
    let pids: Vec<u32> = workers_now.iter().map(|w| w.0).collect();

    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let run = finish(child, Duration::from_secs(60));

    assert_eq!(workers.len(), 2, "{workers:?}\n{}", run.context());
    assert_eq!(kinds, vec!["driver", "tools"], "{}", run.context());
    for (worker, kind, argv, pgid) in &workers_now {
        assert_eq!(argv.len(), 3, "{argv:?}");
        assert_eq!(argv[1], "__guest-worker");
        assert_eq!(&argv[2], kind);
        assert_ne!(*pgid, mur_pgid, "worker {worker} has its own process group");
        for form in leak_forms(KEY) {
            assert!(!argv.join(" ").contains(&form));
        }
    }
    for worker in &pids {
        assert!(!alive(*worker), "worker {worker} outlived the session");
    }
}

/// A worker's environment cannot be read from outside it: it is non-dumpable, as `mur` is.
#[test]
fn a_worker_environment_is_unreadable() {
    let home = Home::new();
    let upstream = Upstream::start(vec![END_TURN.to_string()]);
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, sleep_lifecycle());
    let child = home.spawn(&capsule);
    let pid = child.id();
    let workers = wait_for_workers(pid, 1, Duration::from_secs(60));
    // Read once the session is running: in the instant between its exec and its first statement
    // a worker is still dumpable, with its already empty environment.
    let started = Instant::now();
    while upstream.heads.lock().unwrap().is_empty() && started.elapsed() < WAIT {
        thread::sleep(Duration::from_millis(50));
    }
    let reads: Vec<_> = workers
        .iter()
        .map(|w| fs::read(format!("/proc/{}/environ", w.0)))
        .collect();
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let run = finish(child, Duration::from_secs(60));
    // A capsule whose only tools are runtime-provided runs the driver worker alone.
    let kinds: Vec<&str> = workers.iter().map(|w| w.1.as_str()).collect();
    assert_eq!(kinds, vec!["driver"], "{}", run.context());
    for read in reads {
        let error = read.expect_err("a worker's environ is not readable");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
    }
}

/// `mur __guest-worker` started by hand, with an environment, refuses to start.
#[test]
fn a_worker_with_an_environment_refuses_to_start() {
    let output = Command::new("env")
        .args(["-i", "FOO=bar"])
        .arg(mur())
        .args(["__guest-worker", "tools"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refused to start")
            && stderr.contains("1 variable(s)")
            && stderr.contains("empty environment"),
        "{stderr}"
    );
    let misuse = Command::new(mur())
        .args(["__guest-worker", "nope"])
        .output()
        .unwrap();
    assert_eq!(misuse.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&misuse.stderr).contains("internal entry started by mur run"));
}

/// Kills the `kind` worker once `mur` has a call in flight in it, and returns the run.
fn kill_mid_task(home: &Home, capsule: &Capsule, kind: &str, settle: Duration) -> (Run, u32) {
    let child = home.spawn(capsule);
    let pid = child.id();
    let started = Instant::now();
    let worker = loop {
        if let Some(worker) = workers_of(pid).into_iter().find(|w| w.1 == kind) {
            break worker.0;
        }
        assert!(started.elapsed() < WAIT, "no {kind} worker appeared");
        thread::sleep(Duration::from_millis(50));
    };
    thread::sleep(settle);
    // SAFETY: plain syscall on a grandchild of this test.
    unsafe {
        libc::kill(worker as libc::pid_t, libc::SIGKILL);
    }
    (finish(child, Duration::from_secs(60)), worker)
}

fn assert_lost(run: &Run, kind: &str, worker: u32, call: &str) {
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("error[E-RUN-065]"))
        .collect();
    assert_eq!(lines.len(), 1, "{}", run.context());
    let line = lines[0];
    assert!(
        line.contains(&format!("the {kind} guest worker (pid {worker})"))
            && line.contains(call)
            && line.contains("killed by signal 9"),
        "{line}"
    );
    let message = line.trim_start_matches("error[E-RUN-065]: ");
    let failed = run.of_type("task_failed");
    assert_eq!(failed.len(), 1, "{}", run.context());
    assert_eq!(failed[0]["cause"], "runtime_error");
    assert_eq!(failed[0]["reason"], message, "{}", failed[0]);
    let types = run.event_types();
    let failed_at = types.iter().position(|t| t == "task_failed").unwrap();
    let end_at = types
        .iter()
        .position(|t| t == "task_end")
        .expect("task_end");
    let session_end_at = types
        .iter()
        .position(|t| t == "session_end")
        .expect("session_end");
    assert!(failed_at < end_at && end_at < session_end_at, "{types:?}");
    assert_eq!(run.of_type("task_end")[0]["exit_status"], "failed");
    // `completion.json` is a delegated child's file; a top-level run ends its records with
    // `session_end`, which `mur trace show` reads as an ended session.
    let trace = run.records_dir().join("trace.jsonl");
    let shown = Command::new(mur())
        .args(["trace", "show", trace.to_str().unwrap()])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(!shown.contains("ended without a session_end"), "{shown}");
}

/// The driver worker killed mid-call ends the task `failed` with `E-RUN-065`.
#[test]
fn killing_the_driver_worker_fails_the_task_and_ends_the_records() {
    let home = Home::new();
    let upstream = Upstream::start(Vec::new());
    let capsule = home.capsule(STALL_DRIVER, &upstream.endpoint, "");
    let (run, worker) = kill_mid_task(&home, &capsule, "driver", Duration::from_secs(3));
    assert_lost(&run, "driver", worker, "driver 'stall-driver'");
    assert!(run.took < Duration::from_secs(75), "{:?}", run.took);
}

/// The tools worker killed mid-call ends the task `failed` with `E-RUN-065`, and the driver
/// worker does not survive the session.
#[test]
fn killing_the_tools_worker_fails_the_task_and_ends_the_records() {
    let home = Home::new();
    let upstream = Upstream::start(vec![common::tool_use_response(
        "toolu_stall",
        "stall-tool",
        json!({}),
    )]);
    let tool = format!("  - name: {STALL_TOOL}\n    version: {VERSION}\n    runtime: tool\n");
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &tool);
    let child = home.spawn(&capsule);
    let pid = child.id();
    let started = Instant::now();
    // The tool's call is in flight once the provider has been asked once and the driver's answer
    // has been read; the stalling tool then sleeps far past this wait.
    while upstream.heads.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < WAIT,
            "the driver never called the provider"
        );
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_secs(10));
    let workers = workers_of(pid);
    let tools = workers
        .iter()
        .find(|w| w.1 == "tools")
        .expect("a tools worker")
        .0;
    let driver = workers
        .iter()
        .find(|w| w.1 == "driver")
        .expect("a driver worker")
        .0;
    // SAFETY: plain syscall on a grandchild of this test.
    unsafe {
        libc::kill(tools as libc::pid_t, libc::SIGKILL);
    }
    let run = finish(child, Duration::from_secs(60));
    assert_lost(&run, "tools", tools, "tool 'stall-tool'");
    assert!(!alive(driver), "the driver worker outlived the session");
}

/// On a shell-granted capsule, whose launch moves `mur` into its own cgroup scope, every worker
/// moves with it: each worker's `/proc/<pid>/cgroup` equals `mur`'s.
#[test]
fn workers_share_the_cgroup_mur_moves_into() {
    // A shell grant needs the capsule's own network namespace, which staging refuses
    // (`E-CAP-005`) on a host that cannot create one.
    if capsule_runtime::skip_without_egress_namespace("workers_share_the_cgroup_mur_moves_into") {
        return;
    }
    let home = Home::new();
    let upstream = Upstream::start(vec![END_TURN.to_string()]);
    let extra = format!(
        "  - name: {ECHO_TOOL}\n    version: {VERSION}\n    runtime: tool\ncapabilities:\n  \
         shell:\n    allow:\n      - bash\n{}",
        sleep_lifecycle()
    );
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &extra);
    let child = home.spawn(&capsule);
    let pid = child.id();
    let workers = wait_for_workers(pid, 2, Duration::from_secs(60));
    // The task has run by the time the provider was asked, so the scope move is done.
    let started = Instant::now();
    while upstream.heads.lock().unwrap().is_empty() && started.elapsed() < WAIT {
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(500));
    let mur_cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    let worker_cgroups: Vec<(u32, std::io::Result<String>)> = workers
        .iter()
        .map(|w| (w.0, fs::read_to_string(format!("/proc/{}/cgroup", w.0))))
        .collect();
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let run = finish(child, Duration::from_secs(60));
    assert_eq!(workers.len(), 2, "{workers:?}\n{}", run.context());
    for (worker, cgroup) in worker_cgroups {
        let cgroup = cgroup
            .unwrap_or_else(|error| panic!("worker {worker} is gone ({error})\n{}", run.context()));
        assert_eq!(cgroup, mur_cgroup, "worker {worker}\n{}", run.context());
    }
}

/// A capsule with a driver, a WASM tool and a hook runs three workers, `driver`, `hooks` and
/// `tools`, each a child of `mur` in its own process group; the hook's call runs in the hooks
/// worker, which holds the session in `on-session-start` while the test looks.
#[test]
fn a_hook_starts_the_hooks_worker() {
    let home = Home::new();
    let upstream = Upstream::start(vec![END_TURN.to_string()]);
    let extra = format!(
        "  - name: {ECHO_TOOL}\n    version: {VERSION}\n    runtime: tool\n{}",
        sleep_hook_entry(30)
    );
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &extra);
    let child = home.spawn(&capsule);
    let pid = child.id();
    let workers = wait_for_workers(pid, 3, Duration::from_secs(60));
    let mur_pgid = pgid_of(pid);
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let run = finish(child, Duration::from_secs(60));
    let kinds: Vec<&str> = workers.iter().map(|w| w.1.as_str()).collect();
    assert_eq!(kinds, vec!["driver", "hooks", "tools"], "{}", run.context());
    for (worker, kind, argv, pgid) in &workers {
        assert_eq!(argv.len(), 3, "{argv:?}");
        assert_eq!(argv[1], "__guest-worker");
        assert_eq!(&argv[2], kind);
        assert_ne!(*pgid, mur_pgid, "worker {worker} has its own process group");
        assert!(!alive(*worker), "worker {worker} outlived the session");
    }
}

/// The hooks worker killed while a hook's call is in flight in it ends the task `failed` with
/// `E-RUN-065`.
#[test]
fn killing_the_hooks_worker_fails_the_task_and_ends_the_records() {
    let home = Home::new();
    let upstream = Upstream::start(vec![END_TURN.to_string()]);
    let capsule = home.capsule(ANTHROPIC, &upstream.endpoint, &sleep_hook_entry(60));
    let (run, worker) = kill_mid_task(&home, &capsule, "hooks", Duration::from_secs(10));
    assert_lost(&run, "hooks", worker, "hook 'sleep-hook'");
}

/// A process-transport capsule's bridged tool call whose tools worker is killed under it ends the
/// task `failed` with `E-RUN-065`: the harness is never left to carry on from a tool error.
#[test]
fn killing_the_tools_worker_under_a_bridged_call_fails_the_task() {
    let home = Home::new();
    let project = TempDir::new().unwrap();
    let harness = common::write_fake_harness(
        &project.path().join("harness.sh"),
        "export FIXTURE_HARNESS_PROFILE=bridge-call\nexport FIXTURE_BRIDGE_CALL_TOOL='stall-tool'\n\
         export FIXTURE_BRIDGE_CALL_ARGS='{}'\n",
    );
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: bridged-capsule\nversion: 0.1.0\nartifacts:\n  - name: {PROCESS_DRIVER}\n    \
             version: {VERSION}\n    runtime: driver\n  - name: {STALL_TOOL}\n    version: \
             {VERSION}\n    runtime: tool\ncapabilities:\n  env:\n    allow: [HOME, PATH, \
             FIXTURE_HARNESS_PROFILE]\n\
             inference:\n  transport: process\n  driver:\n    artifact: {PROCESS_DRIVER}\n  \
             command: {}\n",
            harness.display()
        ),
    )
    .unwrap();
    let capsule = Capsule { project, manifest };
    let (run, worker) = kill_mid_task(&home, &capsule, "tools", Duration::from_secs(15));
    assert_lost(&run, "tools", worker, "the tools guest worker");
}

/// A script capsule's root runs in the driver worker, and the tools worker killed under its
/// registry call ends the run with `E-RUN-065` and a `session_end` of `failed`.
#[test]
fn killing_the_tools_worker_under_a_script_capsules_call_ends_the_run() {
    let home = Home::new();
    let artifacts = TempDir::new().unwrap();
    // The script capsule calls `echo-tool`; this one stalls.
    let stall_echo = common::create_tool_artifact(
        artifacts.path(),
        ECHO_TOOL,
        "0.2.0",
        &common::fixture_path("stall-driver/tool/stall-driver.wasm"),
    );
    common::publish_local(&home.home, &stall_echo).success();
    let project = TempDir::new().unwrap();
    let manifest = project.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!("name: script-capsule\nversion: 0.1.0\nartifacts:\n  - name: {ECHO_TOOL}\n    version: 0.2.0\n"),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    let capsule = Capsule { project, manifest };
    let child = Command::new(mur())
        .env("HOME", home.home.path())
        .current_dir(capsule.project.path())
        .args(["run", "--manifest", capsule.manifest.to_str().unwrap()])
        .arg("--verbose")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let workers = wait_for_workers(pid, 2, Duration::from_secs(60));
    let kinds: Vec<String> = workers.iter().map(|w| w.1.clone()).collect();
    // The root runs once its session has started; its registry call reaches the tools worker
    // moments later, where the stalling tool then sleeps far past this wait.
    let started = Instant::now();
    while !traces_under(home.home.path())
        .iter()
        .any(|trace| fs::read_to_string(trace).is_ok_and(|text| text.contains("\"session_start\"")))
    {
        assert!(started.elapsed() < WAIT, "the script session never started");
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(5));
    let tools = workers.iter().find(|w| w.1 == "tools").map(|w| w.0);
    if let Some(tools) = tools {
        // SAFETY: plain syscall on a grandchild of this test.
        unsafe {
            libc::kill(tools as libc::pid_t, libc::SIGKILL);
        }
    }
    let run = finish(child, Duration::from_secs(60));
    assert_eq!(kinds, vec!["driver", "tools"], "{}", run.context());
    let tools = tools.expect("a tools worker");
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let lines: Vec<&str> = run
        .stderr
        .lines()
        .filter(|line| line.starts_with("error[E-RUN-065]"))
        .collect();
    assert_eq!(lines.len(), 1, "{}", run.context());
    assert!(
        lines[0].contains(&format!("the tools guest worker (pid {tools})"))
            && lines[0].contains("tool 'echo-tool'")
            && lines[0].contains("killed by signal 9"),
        "{}",
        lines[0]
    );
    // A failed script run prints no startup block, so its trace is found under the scratch HOME.
    let traces = traces_under(home.home.path());
    assert_eq!(traces.len(), 1, "{traces:?}\n{}", run.context());
    let trace = common::read_whole_trace(&traces[0]);
    let ends: Vec<&Value> = trace
        .iter()
        .filter(|event| event["event_type"] == "session_end")
        .collect();
    assert_eq!(ends.len(), 1, "{}", run.context());
    assert_eq!(ends[0]["exit_status"], "failed", "{}", ends[0]);
}

/// Every `trace.jsonl` beneath `root`.
fn traces_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
                found.push(path);
            }
        }
    }
    found
}
