//! Reaching a running capsule by session address, and the record under `~/.murmur/running/` that
//! makes it possible.
//!
//! Most cases start the capsule as its own `mur run` process rather than in this one. That is the
//! shape under test — a capsule whose URL scrolled past in a terminal nobody has any more — and it
//! is the only shape a case can `SIGKILL`, give a controlling terminal, or deny a writable home
//! without doing the same to the test binary. Each such case gets its own scratch `HOME`, so an
//! ordinal in one case never counts another case's capsule.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, running, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// A value that exists nowhere but this capsule's environment, so finding it in a record would be
/// unambiguous.
const MARKER_ENV: &str = "MURMUR_TEST_MARKER";
const MARKER_VALUE: &str = "marker-4f19bd0c-must-not-be-recorded";
const API_KEY_ENV: &str = "MURMUR_TEST_API_KEY";
const API_KEY_VALUE: &str = "sk-test-8b1f24ce-must-not-be-recorded";

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
/// One per case: every record lands under `$HOME/.murmur/running/`, and a home shared between
/// cases would mean `@1` naming whichever capsule another case happened to start.
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

/// An agent capsule that sleeps between tasks — the only lifecycle where a session outlives the
/// task that reached it, and therefore the only one an address is worth having for.
fn agent_project(endpoint: &str, name: &str, env_allow: &[&str]) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    let env_block = if env_allow.is_empty() {
        String::new()
    } else {
        format!(
            "  env:\n    allow:\n{}",
            env_allow
                .iter()
                .map(|name| format!("      - {name}\n"))
                .collect::<String>()
        )
    };
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n{env_block}\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n\
             inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
             api_key: ${{{API_KEY_ENV}}}\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    project
}

// ── A capsule running as its own process ──────────────────────────────────────

/// How the capsule process is attached to a terminal, which is what `outlives_launcher` reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Attachment {
    /// Whatever this test binary has, which is neither asserted nor relied on.
    Inherited,
    /// Its own session with no controlling terminal, the shape of `nohup … </dev/null &`.
    NoTerminal,
    /// A pty of its own, the shape of a capsule started in a terminal window.
    Terminal,
}

struct Capsule {
    child: Child,
    home: Arc<TempDir>,
    /// Held so the manifest outlives the process reading it.
    _project: TempDir,
    /// The `mur run --json` startup line: `url`, `pid`, `session_id`, `name`, `version`, `workdir`.
    startup: Value,
    stderr: Arc<Mutex<String>>,
}

impl Capsule {
    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        // The recorded pid first: under a pty the child is `script`, and killing it leaves the
        // capsule holding a port and a scratch home that is about to be removed.
        for (_, record) in records(self.home.path()) {
            if let Some(pid) = record["pid"].as_u64() {
                kill(pid as u32, 9);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mur_binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// Starts `mur run` on `project` and blocks until it has printed where its door is.
fn start_capsule(home: &Arc<TempDir>, project: TempDir, attachment: Attachment) -> Option<Capsule> {
    let mur = mur_binary();
    let manifest = project.path().join("murmur.yaml");
    let mut command = match attachment {
        Attachment::Inherited => std::process::Command::new(&mur),
        Attachment::NoTerminal => {
            let mut command = std::process::Command::new(wrapper_binary("setsid")?);
            command.arg(&mur);
            command
        }
        Attachment::Terminal => {
            let mut command = std::process::Command::new(wrapper_binary("script")?);
            command.args([
                "-qec".to_string(),
                format!(
                    "{} run --manifest {} --json",
                    mur.display(),
                    manifest.display()
                ),
                "/dev/null".to_string(),
            ]);
            command
        }
    };
    if attachment != Attachment::Terminal {
        command
            .args(["run", "--manifest"])
            .arg(&manifest)
            .arg("--json");
    }

    let mut child = command
        .current_dir(project.path())
        .env("HOME", home.path())
        .env(MARKER_ENV, MARKER_VALUE)
        .env(API_KEY_ENV, API_KEY_VALUE)
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

    let stderr_text = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_text);
    let stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut sink = stderr_sink.lock().unwrap();
            sink.push_str(&line);
            sink.push('\n');
        }
    });

    let startup = startup_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("timed out waiting for the capsule to print where its door is");

    Some(Capsule {
        child,
        home: Arc::clone(home),
        _project: project,
        startup,
        stderr: stderr_text,
    })
}

/// A capsule started against `server`, or `None` when this host has no `setsid`/`script`.
fn start_agent(
    home: &Arc<TempDir>,
    server: &common::ScriptedServer,
    name: &str,
    attachment: Attachment,
) -> Option<Capsule> {
    let project = agent_project(&server.endpoint, name, &[MARKER_ENV]);
    start_capsule(home, project, attachment)
}

/// The wrapper binary, or `None` for a host that does not ship it.
fn wrapper_binary(name: &str) -> Option<PathBuf> {
    let found = capsule_runtime::find_on_path(name);
    if found.is_none() {
        eprintln!("[SKIP-HOST] running_address: no `{name}` on PATH");
    }
    found
}

// ── The record directory ──────────────────────────────────────────────────────

fn running_dir(home: &Path) -> PathBuf {
    home.join(".murmur").join("running")
}

/// Every record file under `home`, with its parsed contents.
fn records(home: &Path) -> Vec<(PathBuf, Value)> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(running_dir(home)) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // A file that does not parse is not a record yet, which is what a reader of the real
        // directory concludes too.
        if let Some(parsed) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        {
            found.push((path, parsed));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
}

fn record_for(home: &Path, session_id: &str) -> PathBuf {
    running_dir(home).join(format!("{session_id}.json"))
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A record naming a process this test started, for the two cases that need a record the runtime
/// would never write.
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

fn http_get(addr: &str, path: &str) -> String {
    let stream = TcpStream::connect(addr).expect("should connect");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut writer = &stream;
    writer
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut body = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        body.push_str(&line);
    }
    body
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

fn wait_until<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn kill(pid: u32, signal: i32) {
    let _ = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status();
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home).env_remove("NEXUS_API_KEY");
    command
}

// ── 1. A running capsule is reachable by session address ──────────────────────

/// The acceptance measurement: a second terminal that never saw the URL watches the capsule and
/// stops its task, and the provider confirms the cancel took effect.
#[test]
fn a_session_address_reaches_a_running_capsule_with_no_url() {
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn_response("msg_1", "the first task's answer"),
            tool_call_response("msg_2", "toolu_1", "no-such-tool", json!({})),
            end_turn_response("msg_3", "a turn that must never be asked for"),
        ],
        Duration::from_secs(5),
    );
    let home = driver_home();
    let Some(capsule) = start_agent(&home, &server, "address-happy", Attachment::Inherited) else {
        return;
    };

    // The door is used to put work in front of the capsule and for nothing else. Every command
    // below names the session, which is all a second terminal would have.
    let door = capsule.url();
    submit(&door, "msg-1", "answer once");

    let watched = mur(home.path())
        .args(["watch", "@1"])
        .timeout(Duration::from_secs(20))
        .output()
        .expect("mur watch should run");
    let watched_stdout = String::from_utf8_lossy(&watched.stdout).to_string();
    assert!(
        watched_stdout.contains('['),
        "mur watch @1 printed no capsule event; stdout was {watched_stdout:?}, stderr was {:?}",
        String::from_utf8_lossy(&watched.stderr)
    );

    // A second task, stopped mid-turn: the loop would ask the provider again for the tool result.
    let task_id = submit(
        &door,
        "msg-2",
        "start something that would take another turn",
    );
    wait_for_requests(&server, 2, Duration::from_secs(60));

    let cancelled = mur(home.path())
        .args(["cancel", "@1", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let cancelled = String::from_utf8_lossy(&cancelled).to_string();
    assert!(cancelled.contains(&task_id), "{cancelled}");
    assert!(cancelled.contains("canceled"), "{cancelled}");

    // Longer than the scripted delay plus a whole further turn: the cancel is measured at the
    // provider, not at the command that returned.
    thread::sleep(Duration::from_secs(15));
    assert_eq!(
        server.requests().len(),
        2,
        "the third scripted response was consumed: the loop asked the provider again after the \
         cancel"
    );
}

// ── 2. The record names the process and nothing from the environment ──────────

#[test]
fn the_record_names_the_process_and_holds_nothing_from_the_environment() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let Some(capsule) = start_agent(&home, &server, "address-fields", Attachment::Inherited) else {
        return;
    };

    let path = record_for(home.path(), &capsule.session_id());
    let text = wait_until(Duration::from_secs(30), "the record to be written", || {
        fs::read_to_string(&path).ok()
    });
    let record: Value = serde_json::from_str(&text).expect("the record is JSON");

    let mut keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "capsule_name",
            "capsule_version",
            "outlives_launcher",
            "pid",
            "process_start",
            "session_id",
            "started_at",
            "url",
            "workdir",
        ]
    );

    assert_eq!(record["url"], capsule.url());
    assert_eq!(record["pid"], capsule.startup["pid"]);
    assert_eq!(record["pid"].as_u64().unwrap() as u32, capsule.child.id());
    assert_eq!(record["session_id"], capsule.session_id());
    assert_eq!(record["capsule_name"], "address-fields");

    assert!(
        !text.contains(MARKER_VALUE),
        "the record carries an env.allow value: {text}"
    );
    assert!(
        !text.contains(API_KEY_VALUE),
        "the record carries the API key: {text}"
    );
}

// ── 3. The record and its directory are owner-only ────────────────────────────

#[test]
fn the_record_and_its_directory_are_owner_only() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let Some(first) = start_agent(&home, &server, "address-modes", Attachment::Inherited) else {
        return;
    };

    let path = record_for(home.path(), &first.session_id());
    wait_until(Duration::from_secs(30), "the record to be written", || {
        path.exists().then_some(())
    });
    assert_eq!(mode_of(&running_dir(home.path())), 0o700);
    assert_eq!(mode_of(&path), 0o600);

    // What an earlier umask, a restore or a stray chmod leaves behind must be corrected rather
    // than accepted, so the mode is asserted again after a launch into a widened directory.
    fs::set_permissions(running_dir(home.path()), fs::Permissions::from_mode(0o755)).unwrap();
    let second_project = agent_project(&server.endpoint, "address-modes-again", &[]);
    let Some(second) = start_capsule(&home, second_project, Attachment::Inherited) else {
        return;
    };
    let second_path = record_for(home.path(), &second.session_id());
    wait_until(
        Duration::from_secs(30),
        "the second record to be written",
        || second_path.exists().then_some(()),
    );
    assert_eq!(mode_of(&running_dir(home.path())), 0o700);
    assert_eq!(mode_of(&second_path), 0o600);
}

// ── 5. A record whose process is gone is pruned on read ───────────────────────

#[test]
fn a_killed_capsule_is_reported_as_not_running_and_its_record_removed() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let Some(mut capsule) = start_agent(&home, &server, "address-killed", Attachment::Inherited)
    else {
        return;
    };
    let session_id = capsule.session_id();
    let path = record_for(home.path(), &session_id);
    wait_until(Duration::from_secs(30), "the record to be written", || {
        path.exists().then_some(())
    });

    // SIGKILL: no Drop runs, no farewell is written, and the record outlives the process.
    kill(capsule.child.id(), 9);
    let _ = capsule.child.wait();
    assert!(
        path.exists(),
        "a SIGKILLed capsule cannot remove its own record — that is what makes the record a hint"
    );

    let failure = mur(home.path()).args(["watch", "@1"]).assert().failure();
    let stderr = String::from_utf8_lossy(&failure.get_output().stderr).to_string();
    assert!(stderr.contains("E-RUN-022"), "{stderr}");
    assert!(stderr.contains(&session_id), "{stderr}");
    assert!(stderr.contains("is not running"), "{stderr}");
    assert!(
        !path.exists(),
        "the stale record was not pruned by the read"
    );

    let again = mur(home.path()).args(["watch", "@1"]).assert().failure();
    let stderr = String::from_utf8_lossy(&again.get_output().stderr).to_string();
    assert!(stderr.contains("no capsule is running"), "{stderr}");
}

// ── 6. A reused pid does not resolve ──────────────────────────────────────────

/// The failure a pid-only check cannot catch: the pid is alive and belongs to something else.
#[test]
fn a_pid_reused_by_another_process_does_not_resolve() {
    let home = driver_home();
    let mut unrelated = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("sleep should start");
    let session_id = "ses_0199c4e2f1b7712a9d3e4f5061728394";
    write_record(home.path(), session_id, "127.0.0.1:1", unrelated.id(), "0");
    let path = record_for(home.path(), session_id);

    let failure = mur(home.path()).args(["watch", "@1"]).assert().failure();
    let stderr = String::from_utf8_lossy(&failure.get_output().stderr).to_string();

    assert!(stderr.contains("E-RUN-022"), "{stderr}");
    assert!(stderr.contains(session_id), "{stderr}");
    assert!(stderr.contains("is not running"), "{stderr}");
    assert!(
        !path.exists(),
        "the record naming a reused pid was not pruned"
    );

    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

// ── 7. A live pid behind a silent door is unreachable, and is kept ────────────

#[test]
fn a_live_process_whose_door_is_silent_is_unreachable_and_kept() {
    let home = driver_home();
    let mut unrelated = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("sleep should start");
    let start = running::process_start_token(unrelated.id())
        .expect("the host reports a start time for a process it just created");
    // A port bound and released: nothing is listening on it.
    let silent_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let session_id = "ses_0199c4e2f1b7712a9d3e4f5061728395";
    write_record(
        home.path(),
        session_id,
        &format!("127.0.0.1:{silent_port}"),
        unrelated.id(),
        &start,
    );
    let path = record_for(home.path(), session_id);

    let failure = mur(home.path()).args(["watch", "@1"]).assert().failure();
    let stderr = String::from_utf8_lossy(&failure.get_output().stderr).to_string();

    assert!(stderr.contains("E-RUN-023"), "{stderr}");
    assert!(stderr.contains("did not answer"), "{stderr}");
    assert!(
        !stderr.contains("is not running"),
        "a quiet door must not read as a gone process: {stderr}"
    );
    assert!(
        path.exists(),
        "a record naming a live process was pruned because its door was quiet"
    );

    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

// ── 8. Foreground and detached launches are both recorded, and told apart ─────

#[test]
fn a_terminal_backed_launch_and_a_detached_one_are_both_recorded() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);

    let detached_home = driver_home();
    let Some(detached) = start_agent(
        &detached_home,
        &server,
        "address-detached",
        Attachment::NoTerminal,
    ) else {
        return;
    };
    let detached_record = wait_until(
        Duration::from_secs(30),
        "the detached capsule's record",
        || records(detached_home.path()).into_iter().next(),
    )
    .1;
    assert_eq!(
        detached_record["outlives_launcher"], true,
        "a capsule with no controlling terminal outlives whoever started it: {detached_record}"
    );
    drop(detached);

    let attached_home = driver_home();
    let Some(attached) = start_agent(
        &attached_home,
        &server,
        "address-attached",
        Attachment::Terminal,
    ) else {
        return;
    };
    let attached_record = wait_until(
        Duration::from_secs(30),
        "the terminal-backed capsule's record",
        || records(attached_home.path()).into_iter().next(),
    )
    .1;
    assert_eq!(
        attached_record["outlives_launcher"], false,
        "a capsule holding a controlling terminal dies with that window: {attached_record}"
    );
    drop(attached);
}

// ── 9. An unwritable home warns and the run proceeds ──────────────────────────

/// A directory held at `0500` for as long as this value lives, restored afterwards so the scratch
/// home can be removed.
struct ClosedDir(PathBuf);

impl ClosedDir {
    fn at(path: PathBuf) -> Self {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
        Self(path)
    }
}

impl Drop for ClosedDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

/// Root is excluded: a `0500` directory is still writable to it, so there would be nothing to warn
/// about.
fn running_as_root() -> bool {
    let probe = tempfile::tempdir().unwrap();
    let closed = probe.path().join("closed");
    fs::create_dir(&closed).unwrap();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o500)).unwrap();
    let writable = fs::write(closed.join("probe"), b"x").is_ok();
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    writable
}

#[test]
fn a_home_that_cannot_be_written_warns_and_the_capsule_still_serves() {
    if running_as_root() {
        eprintln!("[SKIP-HOST] running_address: a 0500 directory is writable to this user");
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let project = agent_project(&server.endpoint, "address-readonly-home", &[]);

    // Held closed until the warning has been seen: restoring the mode the moment the door opened
    // would race the record write this case is about.
    let closed = ClosedDir::at(home.path().join(".murmur"));
    let Some(capsule) = start_capsule(&home, project, Attachment::Inherited) else {
        return;
    };
    let stderr = wait_until(Duration::from_secs(60), "the W-SEC-023 warning", || {
        let stderr = capsule.stderr();
        stderr.contains("W-SEC-023").then_some(stderr)
    });
    drop(closed);

    // The door is open and answering, which is the whole of "the run proceeds".
    let card = http_get(&capsule.url(), "/.well-known/agent-card.json");
    assert!(card.contains("address-readonly-home"), "{card}");
    assert!(
        stderr.contains(".murmur/running"),
        "the warning must name the path it could not write: {stderr}"
    );
    assert!(
        stderr.contains("session address"),
        "the warning must say what the session has lost: {stderr}"
    );
    assert!(records(home.path()).is_empty());
}

/// With no home to resolve at all, `mur run` refuses before a session exists, so there is no door
/// to record and nothing to warn about. The artifact store lives under the same home the record
/// does, and a run cannot reach a capsule without it.
#[test]
fn a_run_with_no_home_at_all_refuses_before_a_session_exists() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: homeless-capsule\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/run/components/capsule-sleep-input.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    let mut command = Command::cargo_bin("mur").unwrap();
    let failure = command
        .env_remove("HOME")
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.path())
        .args(["run", "--manifest"])
        .arg(project.path().join("murmur.yaml"))
        .args(["--task", "do nothing"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&failure.get_output().stderr).to_string();
    assert!(
        stderr.contains("home directory"),
        "the refusal must name the home directory it could not resolve: {stderr}"
    );
}

// ── 10. A script capsule writes no record ─────────────────────────────────────

#[test]
fn a_script_capsule_writes_no_record() {
    let home = driver_home();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: script-capsule\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/run/components/capsule-sleep-input.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    let mut child = std::process::Command::new(mur_binary())
        .args(["run", "--manifest"])
        .arg(project.path().join("murmur.yaml"))
        .arg("--task")
        .arg("do nothing for a while")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the script capsule should start");

    // The fixture sleeps for two seconds, so the directory is read while the capsule is running:
    // "never written" and "written then removed" are different claims.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut looked = 0_u32;
    while Instant::now() < deadline {
        assert!(
            records(home.path()).is_empty(),
            "a script capsule binds no door and must record nothing"
        );
        looked += 1;
        thread::sleep(Duration::from_millis(50));
    }
    assert!(looked > 5, "the running capsule was barely looked at");

    let status = child.wait().expect("the script capsule should end");
    assert!(status.success(), "the script capsule failed: {status}");
    assert!(records(home.path()).is_empty());
}

// ── 11. Recorded-session addressing is unchanged ──────────────────────────────

/// `mur trace` resolves over recorded sessions in a workdir, and goes on doing so with nothing
/// running and no record directory in existence.
#[test]
fn recorded_session_addressing_is_untouched() {
    let home = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    for (index, suffix) in ["aa11", "bb22"].iter().enumerate() {
        let session_id = format!("ses_0199c4e2f1b7712a9d3e4f50617283{suffix}");
        let dir = workdir.path().join(&session_id);
        fs::create_dir_all(&dir).unwrap();
        let start = 1000 + index as u64 * 1000;
        fs::write(
            dir.join("trace.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "event_type": "session_start",
                    "session_id": session_id,
                    "timestamp": start,
                    "capsule_name": "recorded-capsule",
                    "capsule_version": "0.1.0",
                    "model": "test-model",
                    "max_turns": 10,
                    "capabilities": [],
                    "tools_declared": []
                }),
                json!({
                    "event_type": "session_end",
                    "session_id": session_id,
                    "timestamp": start + 100,
                    "total_turns": 1,
                    "total_input_tokens": 10,
                    "total_output_tokens": 5,
                    "total_tool_calls": 0,
                    "total_shell_calls": 0,
                    "duration_ms": 100,
                    "exit_status": "ok"
                })
            ),
        )
        .unwrap();
    }

    assert!(!running_dir(home.path()).exists());
    for address in ["@1", "@2", "bb22"] {
        mur(home.path())
            .args(["trace", "show", address, "--workdir"])
            .arg(workdir.path())
            .assert()
            .success();
    }
    assert!(
        !running_dir(home.path()).exists(),
        "resolving a recorded session must not touch the running record directory"
    );
}

// ── 12. The bare-URL positional is gone; --url reaches a capsule ──────────────

#[test]
fn the_positional_is_a_session_address_and_url_is_its_own_flag() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let Some(capsule) = start_agent(&home, &server, "address-url-flag", Attachment::Inherited)
    else {
        return;
    };
    let url = capsule.url();
    wait_until(Duration::from_secs(30), "the record to be written", || {
        record_for(home.path(), &capsule.session_id())
            .exists()
            .then_some(())
    });
    // No address can resolve to this capsule any more, so --url is the only way to it.
    fs::remove_file(record_for(home.path(), &capsule.session_id())).unwrap();

    let failure = mur(home.path()).args(["watch", &url]).assert().failure();
    let stderr = String::from_utf8_lossy(&failure.get_output().stderr).to_string();
    assert!(stderr.contains("E-RUN-022"), "{stderr}");
    assert!(stderr.contains("is not a session address"), "{stderr}");
    for absent in ["fallback", "deprecat", "alias"] {
        assert!(
            !stderr.to_lowercase().contains(absent),
            "the refusal offers a {absent}: {stderr}"
        );
    }

    let watched = mur(home.path())
        .args(["watch", "--url", &url])
        .timeout(Duration::from_secs(10))
        .output()
        .expect("mur watch --url should run");
    assert!(
        String::from_utf8_lossy(&watched.stderr)
            .to_string()
            .is_empty(),
        "mur watch --url reported {:?}",
        String::from_utf8_lossy(&watched.stderr)
    );

    let conflict = mur(home.path())
        .args(["watch", "--url", &url, "@1"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&conflict.get_output().stderr).to_string();
    assert!(
        stderr.contains("cannot be used with"),
        "the two spellings must be refused by argument parsing: {stderr}"
    );
}

// ── 13. The agent card identifies its session ─────────────────────────────────

#[test]
fn the_agent_card_names_the_session_it_answers_for() {
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "unused")]);
    let home = driver_home();
    let Some(capsule) = start_agent(&home, &server, "address-card", Attachment::Inherited) else {
        return;
    };

    let body = http_get(&capsule.url(), "/.well-known/agent-card.json");
    let card: Value = serde_json::from_str(&body).unwrap_or_else(|_| panic!("card was {body}"));

    assert_eq!(card["session_id"], capsule.session_id());
    assert_eq!(card["name"], "address-card");
    assert_eq!(card["version"], "0.1.0");
    assert_eq!(card["url"], capsule.url());
    assert_eq!(card["capabilities"]["streaming"], true);
    assert!(card["capabilities"]["tools"].is_array());
}

// ── 4. A session that ends leaves no record, however it ends ──────────────────

/// The one case that launches in this process: only a caller holding the `Result` can say that a
/// launch which returned `Err` still removed its record, which is what makes removal a `Drop`
/// guard rather than a line at each return.
///
/// `HOME` is set process-wide here and nowhere else in this file — every other case drives a
/// subprocess with its own.
fn in_process_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        path
    })
}

fn stage_in_process(
    registry_home: &Path,
    manifest_path: &Path,
    lifecycle: LifecycleConfig,
) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    let registry = LocalRegistry::new(registry_home.join(".murmur").join("artifacts"));
    stage_session(
        Arc::new(registry),
        StageRequest {
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: Some(lifecycle),
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
        },
    )
    .unwrap()
}

#[test]
fn a_session_that_ends_leaves_no_record_whether_it_succeeded_or_failed() {
    let home = in_process_home();
    let registry_home = driver_home();
    std::env::set_var(API_KEY_ENV, API_KEY_VALUE);

    // A capsule that ends with the task that reached it.
    let server = common::ScriptedServer::start(vec![end_turn_response("msg_1", "done")]);
    let project = agent_project(&server.endpoint, "address-exit", &[]);
    let manifest = project.path().join("murmur.yaml");
    let staged = stage_in_process(
        registry_home.path(),
        &manifest,
        LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Exit,
            queue_depth: 8,
            ..Default::default()
        },
    );
    let session_id = staged.session_id.clone();
    let (url_tx, url_rx) = mpsc::channel::<String>();
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let result = launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        });
        let _ = done_tx.send(result.is_ok());
    });
    let url = url_rx.recv_timeout(Duration::from_secs(120)).unwrap();
    let path = record_for(home, &session_id);
    wait_until(Duration::from_secs(30), "the record to be written", || {
        path.exists().then_some(())
    });
    submit(&url, "msg-1", "finish and go");
    let succeeded = done_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("the session should end after its task");
    assert!(succeeded, "the exiting session reported a failure");
    assert!(
        !path.exists(),
        "a session that exited left its record behind"
    );

    // A launch that fails between binding its door and starting its loop. The session directory
    // is closed to writing after staging, so `trace.jsonl` cannot be opened and `launch_session`
    // returns through a `?` rather than through either of its success returns.
    if running_as_root() {
        eprintln!("[SKIP-HOST] running_address: a 0500 directory is writable to this user");
        return;
    }
    let failing_project = agent_project(&server.endpoint, "address-failed", &[]);
    let failing_manifest = failing_project.path().join("murmur.yaml");
    let staged = stage_in_process(
        registry_home.path(),
        &failing_manifest,
        LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            queue_depth: 8,
            ..Default::default()
        },
    );
    let failing_session_id = staged.session_id.clone();
    let session_workdir = staged.workdir.clone();
    fs::set_permissions(&session_workdir, fs::Permissions::from_mode(0o500)).unwrap();
    let result = launch_session(staged, |_| {});
    fs::set_permissions(&session_workdir, fs::Permissions::from_mode(0o700)).unwrap();

    let failure = result.expect_err("the launch was expected to fail after binding its door");
    assert!(
        failure.to_string().contains("trace.jsonl"),
        "the launch failed before it reached the point this case is about: {failure}"
    );
    assert!(
        !record_for(home, &failing_session_id).exists(),
        "a launch that returned Err left its record behind — removal is a guard, not a line at \
         each return"
    );
}
