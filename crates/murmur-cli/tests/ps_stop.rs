//! Listing the capsules running on this machine, and ending one of them.
//!
//! Every capsule here is its own `mur run` process rather than one launched inside this binary.
//! That is the shape under test — `mur stop` sends `SIGTERM` to the pid a record names, and a
//! record written by an in-process launch names *this* process. It is also the only shape where
//! an ordinal means anything: each case gets its own scratch `HOME`, so `@1` in one case never
//! counts another case's capsule.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn_response(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A turn that asks for a tool, so the loop would make a second request if it were not stopped.
fn tool_call_response(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── Projects and homes ────────────────────────────────────────────────────────

/// A scratch `$HOME` with the fixture inference driver published into it.
///
/// One per case. Records land under `$HOME/.murmur/running/`, and a home shared between cases
/// would mean `mur stop @1` in one case signalling whatever another case happened to start.
fn driver_home() -> Arc<TempDir> {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    let mut publish = Command::cargo_bin("mur").unwrap();
    publish
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", driver.to_str().unwrap()])
        .assert()
        .success();
    Arc::new(home)
}

/// An agent capsule that sleeps between tasks, with `extra` spliced into its capability block.
fn agent_project(endpoint: &str, name: &str, extra: &str) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n{extra}\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n  \
             shell_grace_secs: 1\n\
             inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
             api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    project
}

/// The capability and lifecycle block a capsule needs to demote a long shell command.
fn shell_extra() -> &'static str {
    "  shell:\n    allow:\n      - bash\n      - sleep\n"
}

// ── A capsule running as its own process ──────────────────────────────────────

struct Capsule {
    child: Child,
    /// Held so the manifest outlives the process reading it.
    _project: TempDir,
    /// The `mur run --json` startup line: `url`, `pid`, `session_id`, `name`, `version`, `workdir`.
    startup: Value,
}

impl Capsule {
    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn pid(&self) -> u32 {
        self.startup["pid"].as_u64().unwrap() as u32
    }

    fn trace_path(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap()).join("trace.jsonl")
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        // A case that ended the capsule on purpose has nothing here to kill; one that failed
        // halfway leaves a process holding a port and a scratch home about to be removed.
        kill(self.pid(), 9);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts `mur run` on `project` and blocks until it has printed where its door is.
fn start_agent(home: &Arc<TempDir>, server: &common::ScriptedServer, name: &str) -> Capsule {
    start_agent_with(home, server, name, "")
}

fn start_agent_with(
    home: &Arc<TempDir>,
    server: &common::ScriptedServer,
    name: &str,
    extra: &str,
) -> Capsule {
    let project = agent_project(&server.endpoint, name, extra);
    let manifest = project.path().join("murmur.yaml");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["run", "--manifest"])
        .arg(&manifest)
        .arg("--json")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mur run should start");

    let (startup_tx, startup_rx) = mpsc::channel::<Value>();
    let stdout = child.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                if value.get("url").is_some() {
                    let _ = startup_tx.send(value);
                }
            }
        }
    });
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[capsule] {line}");
        }
    });

    let startup = startup_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("timed out waiting for the capsule to print where its door is");

    Capsule {
        child,
        _project: project,
        startup,
    }
}

// ── The record directory ──────────────────────────────────────────────────────

fn running_dir(home: &Path) -> PathBuf {
    home.join(".murmur").join("running")
}

fn record_for(home: &Path, session_id: &str) -> PathBuf {
    running_dir(home).join(format!("{session_id}.json"))
}

/// A record naming a process this case started, for the cases that need one the runtime would
/// never write.
fn write_record(home: &Path, session_id: &str, url: &str, pid: u32, process_start: &str) {
    let dir = running_dir(home);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{session_id}.json")),
        serde_json::to_vec_pretty(&json!({
            "session_id": session_id,
            "url": url,
            "pid": pid,
            "process_start": process_start,
            "capsule_name": "fabricated",
            "capsule_version": "0.1.0",
            "workdir": "/tmp/fabricated",
            "outlives_launcher": true,
            "started_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A 36-character session id ending in `suffix`, so a fabricated record sorts predictably against
/// its siblings.
fn fabricated_id(suffix: &str) -> String {
    let body = format!("{:0>32}", suffix);
    format!("ses_{body}")
}

// ── Host processes a case can safely signal ───────────────────────────────────

/// A long-lived process of this case's own, and the start-time token the host reports for it.
struct Stray {
    child: Child,
}

impl Stray {
    /// `sleep 300`, which ends on `SIGTERM`.
    fn sleeping() -> Self {
        Self {
            child: std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .expect("sleep should start"),
        }
    }

    /// A process that ignores `SIGTERM`, so only `SIGKILL` ends it.
    fn deaf_to_term() -> Self {
        Self {
            child: std::process::Command::new("sh")
                .args(["-c", "trap \"\" TERM; sleep 300"])
                .spawn()
                .expect("sh should start"),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn token(&self) -> String {
        // Let the kernel publish the process before its start time is read.
        thread::sleep(Duration::from_millis(200));
        capsule_runtime::running::process_start_token(self.pid())
            .expect("a freshly started process has a start time")
    }

    /// Reaped through this case's own child handle: a signalled `sleep` stays in the process
    /// table as a zombie until it is waited on, and a `/proc` or `kill -0` probe would read that
    /// as alive.
    fn is_alive(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the child is waitable")
            .is_none()
    }
}

impl Drop for Stray {
    fn drop(&mut self) {
        kill(self.pid(), 9);
        let _ = self.child.wait();
    }
}

// ── Talking to the door ───────────────────────────────────────────────────────

fn http_post_json(addr: &str, body: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = &stream;
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    writer.write_all(request.as_bytes()).unwrap();
    writer.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut response_body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        response_body.push_str(&line);
    }
    serde_json::from_str(&response_body).unwrap_or_else(|_| json!({"_raw": response_body}))
}

fn submit(addr: &str, message_id: &str, text: &str) -> String {
    let response = http_post_json(
        addr,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {"message": {
                "messageId": message_id,
                "role": "user",
                "parts": [{"text": text}]
            }}
        })
        .to_string(),
    );
    response["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the door did not accept the task: {response}"))
        .to_string()
}

fn tasks_get(addr: &str, task_id: &str) -> Value {
    http_post_json(
        addr,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}})
            .to_string(),
    )
}

fn session_stop(addr: &str) -> Value {
    http_post_json(
        addr,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "session/stop", "params": {}}).to_string(),
    )
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn kill(pid: u32, signal: i32) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status();
}

/// Whether this user may signal anything. Root is excluded from the `EPERM` case: there is no pid
/// it cannot signal, so there would be nothing to report.
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
        .unwrap_or(false)
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home).env_remove("NEXUS_API_KEY");
    command
}

fn ps_stdout(home: &Path) -> String {
    let output = mur(home).arg("ps").assert().success().get_output().clone();
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Every line of a `mur ps` listing below the header.
fn ps_rows(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with("SESSION"))
        .collect()
}

fn wait_for_requests(server: &common::ScriptedServer, count: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while server.requests().len() < count {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} provider request(s); saw {}",
            server.requests().len()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn read_trace(trace_path: &Path) -> Vec<Value> {
    fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn wait_for_trace(
    trace_path: &Path,
    timeout: Duration,
    what: &str,
    predicate: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let events = read_trace(trace_path);
        if predicate(&events) {
            return events;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

// ── 1. `mur ps` — happy path ──────────────────────────────────────────────────

/// One running capsule, every column of its row, and the record left exactly as it was: listing
/// a live capsule reads the record and disturbs nothing.
#[test]
fn ps_lists_a_running_capsule_in_full() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "hello")]);
    let home = driver_home();
    let capsule = start_agent(&home, &server, "ps-happy");

    let stdout = ps_stdout(home.path());
    let header = stdout.lines().next().expect("a header line");
    for column in ["SESSION", "CAPSULE", "STATUS", "DETACHED", "UPTIME", "URL"] {
        assert!(header.contains(column), "header lacks {column}: {header:?}");
    }

    let rows = ps_rows(&stdout);
    assert_eq!(rows.len(), 1, "expected exactly one row in:\n{stdout}");
    let row = rows[0];
    let session_id = capsule.session_id();
    assert_eq!(session_id.len(), 36, "a session id is 36 characters");
    assert!(row.contains(&session_id), "{row}");
    assert!(row.contains("ps-happy@0.1.0"), "{row}");
    assert!(row.contains("running"), "{row}");
    assert!(row.contains(&capsule.url()), "{row}");

    // `DETACHED` says yes or no and nothing else, whichever this launch turned out to be.
    let detached = row.contains(" yes ") || row.contains(" no ");
    assert!(detached, "no DETACHED column value in: {row}");

    // The uptime column parses as an elapsed clock.
    let uptime = row
        .split_whitespace()
        .find(|field| field.len() == 8 && field.matches(':').count() == 2)
        .unwrap_or_else(|| panic!("no HH:MM:SS uptime in: {row}"));
    for part in uptime.split(':') {
        assert!(part.parse::<u32>().is_ok(), "{uptime}");
    }

    assert!(
        record_for(home.path(), &session_id).exists(),
        "listing a live capsule must not disturb its record"
    );
}

// ── 2. A machine running nothing ──────────────────────────────────────────────

/// An absent record directory and an empty one are the same fact about the machine, and are
/// reported in the same words.
#[test]
fn ps_on_a_machine_running_nothing_says_so_either_way() {
    let home = tempfile::tempdir().unwrap();
    assert!(!running_dir(home.path()).exists());
    assert_eq!(ps_stdout(home.path()), "no running capsules\n");

    fs::create_dir_all(running_dir(home.path())).unwrap();
    assert_eq!(ps_stdout(home.path()), "no running capsules\n");
}

// ── 3. Pruning the dead, keeping the unreachable ──────────────────────────────

/// A quiet door is not evidence the process is gone. The record whose pid nothing holds is
/// unlinked; the one naming a live process behind a dead port is listed as `unreachable` and kept.
#[test]
fn ps_prunes_the_dead_and_keeps_the_unreachable() {
    let home = tempfile::tempdir().unwrap();
    let mut stray = Stray::sleeping();

    let dead = fabricated_id("dead");
    let quiet = fabricated_id("beef");
    // A pid nothing holds: the kernel never hands this one out.
    write_record(home.path(), &dead, "127.0.0.1:1", 0x7FFF_FFFE, "1");
    write_record(
        home.path(),
        &quiet,
        "127.0.0.1:1",
        stray.pid(),
        &stray.token(),
    );

    let stdout = ps_stdout(home.path());
    let rows = ps_rows(&stdout);
    assert_eq!(rows.len(), 1, "expected one row in:\n{stdout}");
    assert!(rows[0].contains(&quiet), "{stdout}");
    assert!(rows[0].contains("unreachable"), "{stdout}");

    assert!(!record_for(home.path(), &dead).exists(), "the dead record");
    assert!(
        record_for(home.path(), &quiet).exists(),
        "unlinking an unreachable record would throw away the only handle to a slow capsule"
    );

    // A second read is the same read: nothing about the first changed the answer.
    let again = ps_stdout(home.path());
    assert_eq!(ps_rows(&again).len(), 1, "{again}");
    assert!(again.contains(&quiet), "{again}");
    assert!(stray.is_alive(), "`mur ps` must signal nothing");
}

// ── 4. Ordinal order ──────────────────────────────────────────────────────────

/// The first row is what `@1` names. A listing whose first row was not the capsule `mur stop @1`
/// ends would make the two commands disagree about which one is the recent one.
#[test]
fn the_first_row_is_what_stop_at_one_ends() {
    let server = common::ScriptedServer::start(vec![
        end_turn_response("msg_1", "one"),
        end_turn_response("msg_2", "two"),
    ]);
    let home = driver_home();
    let first = start_agent(&home, &server, "ps-older");
    let second = start_agent(&home, &server, "ps-newer");

    let stdout = ps_stdout(home.path());
    let rows = ps_rows(&stdout);
    assert_eq!(rows.len(), 2, "{stdout}");

    // Session ids are time-ordered, so the later launch sorts first.
    let (newer, older) = if second.session_id() > first.session_id() {
        (second.session_id(), first.session_id())
    } else {
        (first.session_id(), second.session_id())
    };
    assert!(
        rows[0].contains(&newer),
        "the first row must be @1: {stdout}"
    );
    assert!(rows[1].contains(&older), "{stdout}");

    let stopped = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stopped = String::from_utf8_lossy(&stopped).to_string();
    assert!(
        stopped.contains(&newer),
        "`mur stop @1` ended a session the first row did not name: {stopped}"
    );

    let after = ps_stdout(home.path());
    assert!(!after.contains(&newer), "{after}");
    assert!(
        after.contains(&older),
        "the other capsule must survive: {after}"
    );
}

// ── 5. The door is asked before the process is signalled ──────────────────────

/// The acceptance measurement for the step order: the task reaches `canceled` and the trace says
/// so, which only happens if the door was asked while the process was still there.
#[test]
fn stop_cancels_through_the_door_before_it_signals() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "no-such-tool", json!({})),
            end_turn_response("msg_2", "a turn that must never be asked for"),
        ],
        Duration::from_secs(20),
    );
    let home = driver_home();
    let capsule = start_agent(&home, &server, "stop-cancels");
    let trace_path = capsule.trace_path();

    let task_id = submit(&capsule.url(), "msg-1", "start something slow");
    wait_for_requests(&server, 1, Duration::from_secs(60));

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(stdout.contains(&capsule.session_id()), "{stdout}");
    assert!(
        stdout.contains(&format!("canceled: {task_id}")),
        "the stop must name the task it cancelled: {stdout}"
    );

    let events = wait_for_trace(
        &trace_path,
        Duration::from_secs(30),
        "the cancellation to reach the trace",
        |events| {
            events
                .iter()
                .any(|event| event["event_type"] == "task_canceled")
        },
    );
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "task_canceled"
                && event["task_id"] == task_id.as_str()),
        "no task_canceled for {task_id}"
    );
    let end = wait_for_trace(
        &trace_path,
        Duration::from_secs(30),
        "the task to end",
        |events| events.iter().any(|event| event["event_type"] == "task_end"),
    );
    assert!(
        end.iter()
            .any(|event| event["event_type"] == "task_end" && event["exit_status"] == "canceled"),
        "the task did not end as canceled: {end:?}"
    );

    // Longer than the scripted delay plus a whole further turn, so a loop that kept going
    // would have reached the provider by now.
    thread::sleep(Duration::from_secs(5));
    assert_eq!(
        server.requests().len(),
        1,
        "the loop asked the provider again after the stop"
    );
    assert!(
        !record_for(home.path(), &capsule.session_id()).exists(),
        "the stop must unlink the record; nothing else will"
    );
    assert_eq!(ps_stdout(home.path()), "no running capsules\n");
}

// ── 6. What the capsule left running ──────────────────────────────────────────

/// A detached shell command is named, not killed. The line is the one `mur cancel` prints for the
/// same item, because both go through one printer.
#[test]
fn stop_names_the_detached_command_it_leaves_running() {
    if common::skip_without_host_support("stop_names_the_detached_command_it_leaves_running") {
        return;
    }
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "bash", json!({"command": "sleep 120"})),
            end_turn_response("msg_2", "never delivered"),
        ],
        Duration::from_secs(30),
    );
    let home = driver_home();
    let capsule = start_agent_with(&home, &server, "stop-residue", shell_extra());
    let trace_path = capsule.trace_path();

    submit(&capsule.url(), "msg-1", "start the long build");
    wait_for_trace(
        &trace_path,
        Duration::from_secs(120),
        "the command to be demoted",
        |events| {
            events
                .iter()
                .any(|event| event["event_type"] == "shell_detached")
        },
    );

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output).to_string();
    let residue_line = stdout
        .lines()
        .find(|line| line.starts_with("running:"))
        .unwrap_or_else(|| panic!("no residue line in:\n{stdout}"));
    assert!(residue_line.contains("wrk_"), "{residue_line}");
    assert!(residue_line.contains("detached shell"), "{residue_line}");
    assert!(residue_line.contains("sleep 120"), "{residue_line}");

    // Naming it is the point: nothing here killed it.
    let still_running = std::process::Command::new("ps")
        .args(["-eo", "args"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("sleep 120"))
        .unwrap_or(false);
    assert!(
        still_running,
        "the detached command was killed; `mur stop` names what it leaves running"
    );
}

// ── 7. Three residue outcomes, not two ────────────────────────────────────────

/// A capsule that answered with nothing running says so, in words an unanswered door could not
/// produce.
#[test]
fn a_capsule_with_no_residue_says_it_left_nothing() {
    let server = common::ScriptedServer::start_with_delay(
        vec![end_turn_response("msg_1", "never delivered")],
        Duration::from_secs(20),
    );
    let home = driver_home();
    let capsule = start_agent(&home, &server, "stop-empty");
    submit(&capsule.url(), "msg-1", "nothing else running");
    wait_for_requests(&server, 1, Duration::from_secs(60));

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output).to_string();
    assert!(
        stdout.contains("residue: nothing else was left running"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("could not be asked"),
        "an answered door must not read as an unanswered one: {stdout}"
    );
    assert!(!stdout.contains("running:"), "{stdout}");
}

// ── 8. A quiet door does not stop the stop ────────────────────────────────────

/// The signalling steps run whether or not the door answered, and the missing account is reported
/// as missing rather than as nothing.
#[test]
fn a_quiet_door_does_not_stop_the_stop() {
    let home = tempfile::tempdir().unwrap();
    let mut stray = Stray::sleeping();
    let session_id = fabricated_id("beef");
    write_record(
        home.path(),
        &session_id,
        "127.0.0.1:1",
        stray.pid(),
        &stray.token(),
    );

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output).to_string();
    assert!(stdout.contains(&session_id), "{stdout}");
    assert!(stdout.contains("signal:  SIGTERM"), "{stdout}");
    assert!(
        stdout.contains("residue: unknown — the capsule could not be asked:"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("nothing else was left running"),
        "an unanswered door must not read as an answered one: {stdout}"
    );

    thread::sleep(Duration::from_millis(300));
    assert!(
        !stray.is_alive(),
        "SIGTERM was not sent despite the silence"
    );
    assert!(
        !record_for(home.path(), &session_id).exists(),
        "the stop unlinks the record; the signalled process never gets to"
    );
}

// ── 9. Escalation to SIGKILL ──────────────────────────────────────────────────

/// A process that ignores `SIGTERM` is escalated to, and the report says which signal ended it.
#[test]
fn a_process_deaf_to_sigterm_is_killed_after_the_grace_period() {
    let home = tempfile::tempdir().unwrap();
    let mut stray = Stray::deaf_to_term();
    let session_id = fabricated_id("f00d");
    write_record(
        home.path(),
        &session_id,
        "127.0.0.1:1",
        stray.pid(),
        &stray.token(),
    );

    let started = Instant::now();
    let output = mur(home.path())
        .args(["stop", "@1", "--timeout", "2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let elapsed = started.elapsed();
    let stdout = String::from_utf8_lossy(&output).to_string();

    assert!(stdout.contains("signal:  SIGKILL"), "{stdout}");
    assert!(
        elapsed < Duration::from_secs(8),
        "--timeout 2 waited out the 10-second default ({elapsed:?})"
    );
    thread::sleep(Duration::from_millis(300));
    assert!(!stray.is_alive(), "SIGKILL did not end it");
}

/// `--timeout 0` escalates with no grace period at all.
#[test]
fn a_zero_timeout_escalates_immediately() {
    let home = tempfile::tempdir().unwrap();
    let mut stray = Stray::deaf_to_term();
    let session_id = fabricated_id("aaaa");
    write_record(
        home.path(),
        &session_id,
        "127.0.0.1:1",
        stray.pid(),
        &stray.token(),
    );

    let started = Instant::now();
    let output = mur(home.path())
        .args(["stop", "@1", "--timeout", "0"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let elapsed = started.elapsed();
    assert!(
        String::from_utf8_lossy(&output).contains("signal:  SIGKILL"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "{elapsed:?} is a grace period"
    );
    thread::sleep(Duration::from_millis(300));
    assert!(!stray.is_alive());
}

// ── 10. The load-bearing invariant ────────────────────────────────────────────

/// A record is a hint. A live pid whose recorded start time no longer matches names a process that
/// inherited the number, and signalling it would end something the operator never launched.
#[test]
fn stop_refuses_to_signal_a_pid_it_cannot_confirm() {
    let home = tempfile::tempdir().unwrap();
    let mut stray = Stray::sleeping();
    let session_id = fabricated_id("cafe");
    write_record(
        home.path(),
        &session_id,
        "127.0.0.1:1",
        stray.pid(),
        "deliberately-not-this-process",
    );

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains("E-RUN-022"), "{stderr}");
    assert!(stderr.contains("is not running"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("stopped:"),
        "nothing was stopped"
    );

    assert!(
        !record_for(home.path(), &session_id).exists(),
        "the record names no running session and is unlinked"
    );
    thread::sleep(Duration::from_millis(300));
    assert!(
        stray.is_alive(),
        "an unrelated process was signalled; a record is a hint, never a licence"
    );
}

// ── 11. An address that names no running session ──────────────────────────────

/// Both spellings of an address nothing answers to fail the same way, and neither reports a
/// session as stopped.
#[test]
fn an_address_naming_nothing_running_is_refused() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(running_dir(home.path())).unwrap();

    for address in ["@1", "ses_00000000000000000000000000000000"] {
        let output = mur(home.path())
            .args(["stop", address])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(stderr.contains("E-RUN-022"), "{address}: {stderr}");
        assert!(!stdout.contains("stopped:"), "{address}: {stdout}");
        assert!(!stdout.contains("running:"), "{address}: {stdout}");
    }
}

// ── 12. `mur stop` and `mur cancel` mean different things ─────────────────────

/// A task ended; the session did not. Then the session ended.
#[test]
fn cancel_ends_a_task_and_stop_ends_the_session() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "no-such-tool", json!({})),
            end_turn_response("msg_2", "never delivered"),
        ],
        Duration::from_secs(20),
    );
    let home = driver_home();
    let capsule = start_agent(&home, &server, "stop-vs-cancel");
    let session_id = capsule.session_id();

    let task_id = submit(&capsule.url(), "msg-1", "start something slow");
    wait_for_requests(&server, 1, Duration::from_secs(60));

    mur(home.path())
        .args(["cancel", "@1", &task_id])
        .assert()
        .success();
    let after_cancel = ps_stdout(home.path());
    assert!(
        after_cancel.contains(&session_id),
        "a cancel ends a task, not a session: {after_cancel}"
    );

    mur(home.path()).args(["stop", "@1"]).assert().success();
    assert_eq!(ps_stdout(home.path()), "no running capsules\n");
}

/// Both commands are listed and their one-line descriptions distinguish task from session.
#[test]
fn help_distinguishes_the_task_command_from_the_session_command() {
    let home = tempfile::tempdir().unwrap();
    let output = mur(home.path())
        .arg("--help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8_lossy(&output).to_string();

    let cancel_line = help
        .lines()
        .find(|line| line.trim_start().starts_with("cancel "))
        .unwrap_or_else(|| panic!("`mur cancel` is not listed:\n{help}"));
    let stop_line = help
        .lines()
        .find(|line| line.trim_start().starts_with("stop "))
        .unwrap_or_else(|| panic!("`mur stop` is not listed:\n{help}"));
    assert!(
        help.lines()
            .any(|line| line.trim_start().starts_with("ps ")),
        "{help}"
    );

    assert!(cancel_line.contains("task"), "{cancel_line}");
    assert!(
        stop_line.contains("capsule") || stop_line.contains("session"),
        "{stop_line}"
    );
    assert!(
        !stop_line.contains("one running task"),
        "the two descriptions must not read alike: {stop_line}"
    );
}

// ── 13. `mur stop` takes no URL ───────────────────────────────────────────────

/// Two of the three steps are `kill(2)` against a local pid, so a URL-addressed stop could only
/// ever do the first one. The spelling is refused rather than silently doing a third of the job.
#[test]
fn stop_refuses_a_url() {
    let home = tempfile::tempdir().unwrap();
    let output = mur(home.path())
        .args(["stop", "--url", "localhost:41235"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains("--url"), "{stderr}");
    assert!(stderr.contains("unexpected argument"), "{stderr}");

    let help = mur(home.path())
        .args(["stop", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8_lossy(&help).to_string();
    assert!(help.contains("SESSION"), "{help}");
    assert!(help.contains("--timeout"), "{help}");
    assert!(help.contains("[default: 10]"), "{help}");
    assert!(!help.contains("--url"), "{help}");
}

// ── 14. `mur watch` says which tool the operator is looking at ────────────────

/// Ctrl-C means opposite things in the two places a capsule is watched from, so the watch says
/// which one this is — on stderr, so piping stdout is unaffected.
#[test]
fn watch_names_the_session_and_what_ctrl_c_does() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "an answer")]);
    let home = driver_home();
    let capsule = start_agent(&home, &server, "watch-banner");
    submit(&capsule.url(), "msg-1", "answer once");

    let watched = mur(home.path())
        .args(["watch", "@1"])
        .timeout(Duration::from_secs(20))
        .output()
        .expect("mur watch should run");
    let stderr = String::from_utf8_lossy(&watched.stderr).to_string();
    let stdout = String::from_utf8_lossy(&watched.stdout).to_string();

    let first = stderr.lines().next().unwrap_or("");
    assert!(first.contains(&capsule.session_id()), "{stderr}");
    assert!(first.contains("Ctrl-C"), "{stderr}");
    assert!(first.contains("not the capsule"), "{stderr}");
    assert!(first.contains("mur stop"), "{stderr}");

    assert!(
        !stdout.contains("Ctrl-C"),
        "the banner must not reach stdout: {stdout}"
    );
    assert!(
        ps_stdout(home.path()).contains(&capsule.session_id()),
        "watching a capsule must not end it"
    );
}

// ── 15. `session/stop` on the wire ────────────────────────────────────────────

/// The method cancels and reports; it does not shut the door down. A second call is not a failure.
#[test]
fn session_stop_answers_and_leaves_the_door_up() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            tool_call_response("msg_1", "toolu_1", "no-such-tool", json!({})),
            end_turn_response("msg_2", "never delivered"),
        ],
        Duration::from_secs(20),
    );
    let home = driver_home();
    let capsule = start_agent(&home, &server, "session-stop-wire");
    let door = capsule.url();

    let task_id = submit(&door, "msg-1", "start something slow");
    wait_for_requests(&server, 1, Duration::from_secs(60));

    let response = session_stop(&door);
    let result = &response["result"];
    assert_eq!(
        result["session_id"],
        capsule.session_id().as_str(),
        "{response}"
    );
    assert_eq!(result["canceled"], json!([task_id]), "{response}");
    assert!(
        result["residue"].is_array(),
        "the residue key is always present: {response}"
    );

    // The process is still running and still answers.
    assert_eq!(
        tasks_get(&door, &task_id)["result"]["status"]["state"],
        "canceled"
    );
    assert!(
        record_for(home.path(), &capsule.session_id()).exists(),
        "session/stop ends no process, so the record stands"
    );

    // Issued again: nothing left to cancel, and that is an answer rather than an error.
    let again = session_stop(&door);
    assert!(again.get("error").is_none(), "{again}");
    assert_eq!(
        again["result"]["canceled"].as_array().map(Vec::len),
        Some(0),
        "{again}"
    );
    assert!(again["result"]["residue"].is_array(), "{again}");
}

/// A capsule with nothing running still emits `residue`, as an empty array — the one place this
/// differs from `tasks/cancel`, which omits the key. A session stop has to be able to say
/// "nothing" as a positive fact.
#[test]
fn session_stop_always_emits_the_residue_key() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "an answer")]);
    let home = driver_home();
    let capsule = start_agent(&home, &server, "session-stop-empty");

    let response = session_stop(&capsule.url());
    assert_eq!(
        response["result"]["residue"],
        json!([]),
        "nothing running must still be said: {response}"
    );
}

// ── 16. A session that cannot be ended ────────────────────────────────────────

/// A pid this user may not signal is reported, and its record is kept: the capsule it names is
/// still running, and removing the only handle to it would be the opposite of what was asked for.
#[test]
fn a_session_that_cannot_be_signalled_is_reported_and_its_record_kept() {
    if running_as_root() {
        eprintln!("[SKIP-HOST] ps_stop: running as root, every pid is signallable");
        return;
    }
    let Some(token) = capsule_runtime::running::process_start_token(1) else {
        eprintln!("[SKIP-HOST] ps_stop: pid 1's start time is unreadable here");
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let session_id = fabricated_id("bbbb");
    write_record(home.path(), &session_id, "127.0.0.1:1", 1, &token);

    let output = mur(home.path())
        .args(["stop", "@1"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains("E-RUN-024"), "{stderr}");
    assert!(stderr.contains("could not be ended"), "{stderr}");
    assert!(
        record_for(home.path(), &session_id).exists(),
        "the session is still running; its record must be kept"
    );
}
