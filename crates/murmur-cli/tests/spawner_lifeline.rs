//! A delegated child's spawner lifeline, at the binary: one `mur run` started directly, with a
//! lifeline whose write end this test holds through [`ChildLifeline`], standing in for the parent
//! runtime that would hold it.
//!
//! EOF on the lifeline winds an agent session down as a first `SIGTERM` would, writing
//! `spawner_ended` first; bytes on it are not EOF; a value that is not this session's read end
//! refuses the launch with `E-RUN-020`; a script capsule ends its run with status 143. The
//! process-level cases, where a real parent is killed outright, are in `spawner_lifeline_kill.rs`.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use capsule_runtime::delegation::{SpawnerHandle, SPAWNER_ENV};
use capsule_runtime::formation::{FORMATION_ID_ENV, FORMATION_PEERS_ENV};
use capsule_runtime::{
    ChildLifeline, FormationId, MemberLifeline, FORMATION_LIFELINE_ENV, SPAWNER_LIFELINE_ENV,
};
use common::door_capsule::{
    agent_project, driver_home, end_turn, message, rpc, wait_for_requests, AUTHENTICATION_YAML,
    QUEUE_SLEEP_YAML,
};
use common::{
    event_kinds as kinds, read_whole_trace as read_trace, sessions_under, signal, ScriptedServer,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// How long a session has to exit once its lifeline is closed: the runtime's own teardown
/// deadline, with room for a loaded host.
const EXIT_LIMIT: Duration = Duration::from_secs(30);

/// How long a session has to print its readiness line.
const READY_LIMIT: Duration = Duration::from_secs(120);

const SPAWNER_CLOSED: &str = "spawner lifeline closed — the session that delegated to this one \
                              has ended";
const FORMATION_CLOSED: &str = "formation lifeline closed";

/// A queue/sleep agent with an authenticated door, against `server`.
fn child_project(server: &ScriptedServer) -> TempDir {
    agent_project(
        &server.endpoint,
        "worker",
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    )
}

/// `mur run --manifest <project>/murmur.yaml --json <args>` under `home`, with no lifeline,
/// formation variable, spawner handle, door token or provider key inherited from this process.
fn command(home: &Path, project: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .args(["run", "--manifest"])
        .arg(project.join("murmur.yaml"))
        .arg("--json")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
        .env_remove(FORMATION_ID_ENV)
        .env_remove(FORMATION_PEERS_ENV)
        .env_remove(FORMATION_LIFELINE_ENV)
        .env_remove(SPAWNER_LIFELINE_ENV)
        .env_remove(SPAWNER_ENV);
    command
}

/// A spawner handle naming a parent session and delegation, reporting to nobody.
fn handle() -> SpawnerHandle {
    SpawnerHandle {
        session_id: "ses_0199c4e2f1b7712a9d3e4f5061728parent".to_string(),
        context_id: "ctx_spawner_lifeline".to_string(),
        delegation_id: "dlg_0199c4e2f1b7712a9d3e4f50617lifeline".to_string(),
        report_to: None,
    }
}

/// What a session is started with.
#[derive(Default)]
struct Start<'a> {
    /// Hand it a spawner lifeline this test holds the write end of.
    spawner: bool,
    /// Hand it a formation lifeline too, in a formation of its own.
    formation: bool,
    /// Set `MURMUR_SPAWNER` to [`handle`].
    lineage: bool,
    env: &'a [(&'a str, &'a str)],
}

/// A session started by this test, read as it runs.
struct Session {
    child: Child,
    startup: Value,
    stderr: Arc<Mutex<String>>,
    spawner: Option<ChildLifeline>,
    formation: Option<(FormationId, MemberLifeline)>,
    _project: TempDir,
}

impl Session {
    fn start(home: &Path, project: TempDir, start: Start<'_>) -> Self {
        let mut command = command(home, project.path(), &[]);
        command
            .envs(start.env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if start.lineage {
            command.env(SPAWNER_ENV, handle().to_env_value());
        }
        let mut formation = start.formation.then(|| {
            let id = FormationId::mint();
            command.env(FORMATION_ID_ENV, id.as_str());
            let lifeline = MemberLifeline::new().unwrap();
            lifeline.hand_to(&mut command);
            (id, lifeline)
        });
        let mut spawner = start.spawner.then(|| ChildLifeline::new().unwrap());
        if let Some(lifeline) = &spawner {
            lifeline.hand_to(&mut command);
        }
        let mut child = command.spawn().unwrap();
        if let Some(lifeline) = &mut spawner {
            lifeline.spawned();
        }
        if let Some((_, lifeline)) = &mut formation {
            lifeline.spawned();
        }

        let (line_tx, lines) = mpsc::channel::<String>();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = line_tx.send(line);
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let err = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[child] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        let line = lines.recv_timeout(READY_LIMIT).unwrap_or_else(|_| {
            panic!(
                "the session printed no readiness line; stderr:\n{}",
                stderr.lock().unwrap()
            )
        });
        Self {
            child,
            startup: serde_json::from_str(&line).unwrap(),
            stderr,
            spawner,
            formation,
            _project: project,
        }
    }

    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn token(&self) -> String {
        self.startup["tokens"]["operator"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn session_dir(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap())
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Close the spawner lifeline's write end: the session reads EOF.
    fn close_spawner(&mut self) {
        self.spawner
            .as_mut()
            .expect("the session was handed a spawner lifeline")
            .close();
    }

    /// Close the formation lifeline's write end.
    fn close_formation(&mut self) {
        self.formation
            .as_mut()
            .expect("the session was handed a formation lifeline")
            .1
            .close();
    }

    fn exited_within(&mut self, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn assert_exits_within(&mut self, limit: Duration) -> ExitStatus {
        self.exited_within(limit).unwrap_or_else(|| {
            panic!(
                "the session was still running {limit:?} later; stderr:\n{}",
                self.stderr()
            )
        })
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.session_dir().join("trace.jsonl"))
    }

    /// `message/send` under the operator token; the task id.
    fn send_task(&self) -> String {
        let sent = rpc(
            &self.url(),
            Some(&self.token()),
            "message/send",
            message("m-held", "hold"),
        );
        assert_eq!(sent.status, 200, "{sent:?}");
        sent.json()["result"]["id"].as_str().unwrap().to_string()
    }

    fn door_session_id(&self) -> String {
        let card = rpc(
            &self.url(),
            Some(&self.token()),
            "agent/getAuthenticatedExtendedCard",
            json!({}),
        );
        assert_eq!(card.status, 200, "{card:?}");
        card.body
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn position(events: &[Value], kind: &str) -> Option<usize> {
    events.iter().position(|event| event["event_type"] == kind)
}

/// A session wound down because its spawner ended: exactly one `spawner_ended`, carrying its own
/// ids and timestamp, ahead of every record the wind-down writes, no `formation_ended`, and
/// `session_end` last. Returns the record.
fn assert_wound_down_by_spawner(events: &[Value]) -> Value {
    let kinds = kinds(events);
    let ended: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "spawner_ended")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(ended.len(), 1, "{kinds:?}");
    let record = &events[ended[0]];
    assert!(record["event_id"].as_str().is_some(), "{record}");
    assert!(record["parent_id"].as_str().is_some(), "{record}");
    assert!(record["timestamp"].as_u64().is_some(), "{record}");
    for (index, event) in events.iter().enumerate() {
        let wound_down = match kinds[index] {
            "task_canceled" | "task_rejected" | "shell_abandoned" => true,
            "task_end" => event["exit_status"] == "canceled",
            _ => false,
        };
        if wound_down {
            assert!(index > ended[0], "{kinds:?}");
        }
    }
    assert!(!kinds.contains(&"formation_ended"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"session_end"), "{kinds:?}");
    record.clone()
}

// ── An agent session winds down on EOF ────────────────────────────────────────

/// An idle child whose spawner ends writes `spawner_ended` then `session_end` and exits, with no
/// `SIGTERM` sent. Without a spawner handle the record names no lineage; with one it names the
/// handle's session and delegation, as `session_start` does.
#[test]
fn an_idle_child_winds_down_when_its_spawner_lifeline_closes() {
    let home = driver_home();
    for lineage in [false, true] {
        let server = ScriptedServer::start(Vec::new());
        let mut session = Session::start(
            home.path(),
            child_project(&server),
            Start {
                spawner: true,
                lineage,
                ..Start::default()
            },
        );
        assert!(session.door_session_id().contains(&session.session_id()));

        let closed = Instant::now();
        session.close_spawner();
        let status = session.assert_exits_within(EXIT_LIMIT);
        eprintln!(
            "[measure] idle child, lineage {lineage}: EOF to exit {} ms ({status})",
            closed.elapsed().as_millis()
        );
        let events = session.trace();
        let record = assert_wound_down_by_spawner(&events);
        let tail = kinds(&events);
        assert_eq!(&tail[tail.len() - 2..], ["spawner_ended", "session_end"]);
        let start = &events[0];
        assert_eq!(start["event_type"], "session_start");
        if lineage {
            assert_eq!(record["spawned_by"], handle().session_id, "{record}");
            assert_eq!(record["delegation_id"], handle().delegation_id, "{record}");
            assert_eq!(record["spawned_by"], start["spawned_by"]);
            assert_eq!(record["delegation_id"], start["delegation_id"]);
        } else {
            assert!(record.get("spawned_by").is_none(), "{record}");
            assert!(record.get("delegation_id").is_none(), "{record}");
            assert!(start.get("spawned_by").is_none(), "{start}");
        }
        let stderr = session.stderr();
        assert!(stderr.contains(SPAWNER_CLOSED), "{stderr}");
        assert!(!stderr.contains("SIGTERM received"), "{stderr}");
        assert!(!stderr.contains("W-RUN-007"), "{stderr}");
    }
}

/// A child mid-task cancels the task — in its inference phase — after `spawner_ended`.
#[test]
fn a_child_mid_task_cancels_it_when_its_spawner_lifeline_closes() {
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let mut session = Session::start(
        home.path(),
        child_project(&server),
        Start {
            spawner: true,
            lineage: true,
            ..Start::default()
        },
    );
    let task_id = session.send_task();
    wait_for_requests(&server, 1);

    session.close_spawner();
    session.assert_exits_within(EXIT_LIMIT);
    let events = session.trace();
    assert_wound_down_by_spawner(&events);
    let ended = position(&events, "spawner_ended").unwrap();
    let canceled = events
        .iter()
        .position(|event| event["event_type"] == "task_canceled" && event["task_id"] == task_id)
        .unwrap_or_else(|| panic!("no task_canceled: {:?}", kinds(&events)));
    assert_eq!(
        events[canceled]["phase"], "inference",
        "{}",
        events[canceled]
    );
    let task_end = events
        .iter()
        .position(|event| {
            event["event_type"] == "task_end"
                && event["task_id"] == task_id
                && event["exit_status"] == "canceled"
        })
        .unwrap_or_else(|| panic!("no canceled task_end: {:?}", kinds(&events)));
    let session_end = position(&events, "session_end").unwrap();
    assert!(
        ended < canceled && canceled < task_end && task_end < session_end,
        "{:?}",
        kinds(&events)
    );
    assert_eq!(server.requests().len(), 1, "the task asked its model again");
}

/// A detached shell command still running when the spawner ends is abandoned by the teardown, as a
/// first `SIGTERM` would abandon it, and its `shell_abandoned` follows `spawner_ended`.
#[test]
fn a_childs_detached_command_is_abandoned_after_spawner_ended() {
    if common::skip_without_host_support(
        "a_childs_detached_command_is_abandoned_after_spawner_ended",
    ) {
        return;
    }
    let home = driver_home();
    let server = ScriptedServer::start(vec![
        common::tool_use_response("toolu_sleep", "bash", json!({"command": "sleep 120"})),
        end_turn(2, "started it"),
    ]);
    let project = agent_project(
        &server.endpoint,
        "worker",
        "  shell:\n    allow: [bash, sleep]\n",
        &format!(
            "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n  \
             shell_grace_secs: 1\n{AUTHENTICATION_YAML}"
        ),
    );
    let mut session = Session::start(
        home.path(),
        project,
        Start {
            spawner: true,
            ..Start::default()
        },
    );
    session.send_task();

    let deadline = Instant::now() + Duration::from_secs(120);
    let work_id = loop {
        let events = session.trace();
        let detached = events
            .iter()
            .find(|event| event["event_type"] == "shell_detached");
        let ended = position(&events, "task_end").is_some();
        if let (Some(detached), true) = (detached, ended) {
            break detached["work_id"].as_str().unwrap().to_string();
        }
        assert!(
            Instant::now() < deadline,
            "the command was never detached: {:?}",
            kinds(&events)
        );
        thread::sleep(Duration::from_millis(100));
    };
    assert!(work_id.starts_with("wrk_"), "{work_id}");

    session.close_spawner();
    session.assert_exits_within(EXIT_LIMIT);
    let events = session.trace();
    assert_wound_down_by_spawner(&events);
    let ended = position(&events, "spawner_ended").unwrap();
    let abandoned = events
        .iter()
        .position(|event| event["event_type"] == "shell_abandoned" && event["work_id"] == work_id)
        .unwrap_or_else(|| panic!("no shell_abandoned for {work_id}: {:?}", kinds(&events)));
    assert!(ended < abandoned, "{:?}", kinds(&events));
}

#[test]
fn bytes_on_a_spawner_lifeline_are_not_its_end() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let mut session = Session::start(
        home.path(),
        child_project(&server),
        Start {
            spawner: true,
            ..Start::default()
        },
    );
    let session_id = session.session_id();

    let write = session.spawner.as_ref().unwrap().write_fd().unwrap();
    let bytes = [b'x'; 64];
    // SAFETY: writes from a live 64-byte array to the write end this test holds.
    #[allow(unsafe_code)]
    let written = unsafe { libc::write(write, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(written, 64);
    let held = Instant::now();
    while held.elapsed() < Duration::from_secs(2) {
        assert!(
            session.child.try_wait().unwrap().is_none(),
            "bytes ended it"
        );
        assert!(session.door_session_id().contains(&session_id));
        thread::sleep(Duration::from_millis(250));
    }

    session.close_spawner();
    session.assert_exits_within(EXIT_LIMIT);
    assert_wound_down_by_spawner(&session.trace());
}

// ── Termination is begun once ─────────────────────────────────────────────────

fn skip_without_the_delay_seam(name: &str) -> bool {
    if !cfg!(debug_assertions) {
        eprintln!("[SKIP] {name}: the task-end delay seam exists only in debug builds");
        return true;
    }
    false
}

/// A session whose in-flight task is slow to record its end once cancelled, so its teardown lasts
/// long enough to signal or close something during it.
fn session_with_a_slow_teardown(
    home: &TempDir,
    server: &ScriptedServer,
    formation: bool,
) -> Session {
    let session = Session::start(
        home.path(),
        child_project(server),
        Start {
            spawner: true,
            formation,
            lineage: true,
            env: &[("MURMUR_DEBUG_TASK_END_DELAY_MS", "2000")],
        },
    );
    session.send_task();
    wait_for_requests(server, 1);
    session
}

#[test]
fn spawner_eof_then_one_sigterm_still_ends_through_the_teardown() {
    if skip_without_the_delay_seam("spawner_eof_then_one_sigterm_still_ends_through_the_teardown") {
        return;
    }
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let mut session = session_with_a_slow_teardown(&home, &server, false);
    session.close_spawner();
    thread::sleep(Duration::from_millis(300));
    signal(session.child.id(), libc::SIGTERM);
    session.assert_exits_within(EXIT_LIMIT);
    let events = session.trace();
    assert_wound_down_by_spawner(&events);
    assert_eq!(
        kinds(&events)
            .iter()
            .filter(|kind| **kind == "session_end")
            .count(),
        1,
        "{:?}",
        kinds(&events)
    );
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "task_end" && event["exit_status"] == "canceled"),
        "the teardown was cut short: {:?}",
        kinds(&events)
    );
}

#[test]
fn sigterm_then_spawner_eof_writes_no_spawner_ended() {
    if skip_without_the_delay_seam("sigterm_then_spawner_eof_writes_no_spawner_ended") {
        return;
    }
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let mut session = session_with_a_slow_teardown(&home, &server, false);
    signal(session.child.id(), libc::SIGTERM);
    thread::sleep(Duration::from_millis(300));
    session.close_spawner();
    session.assert_exits_within(EXIT_LIMIT);
    let events = session.trace();
    assert!(
        position(&events, "spawner_ended").is_none(),
        "{:?}",
        kinds(&events)
    );
    assert_eq!(kinds(&events).last(), Some(&"session_end"));
    let stderr = session.stderr();
    assert!(stderr.contains("SIGTERM received"), "{stderr}");
    assert!(!stderr.contains(SPAWNER_CLOSED), "{stderr}");
}

/// A session holding both lifelines records the end of whichever closed first, and only that.
#[test]
fn with_both_lifelines_only_the_first_closed_is_recorded() {
    if skip_without_the_delay_seam("with_both_lifelines_only_the_first_closed_is_recorded") {
        return;
    }
    let home = driver_home();
    for spawner_first in [true, false] {
        let server =
            ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
        let mut session = session_with_a_slow_teardown(&home, &server, true);
        if spawner_first {
            session.close_spawner();
            thread::sleep(Duration::from_millis(300));
            session.close_formation();
        } else {
            session.close_formation();
            thread::sleep(Duration::from_millis(300));
            session.close_spawner();
        }
        session.assert_exits_within(EXIT_LIMIT);
        let events = session.trace();
        let kinds = kinds(&events);
        let stderr = session.stderr();
        if spawner_first {
            assert_wound_down_by_spawner(&events);
            assert!(stderr.contains(SPAWNER_CLOSED), "{stderr}");
            assert!(!stderr.contains(FORMATION_CLOSED), "{stderr}");
        } else {
            let id = session.formation.as_ref().unwrap().0.as_str().to_string();
            common::assert_wound_down_by_formation(&events, &id);
            assert!(!kinds.contains(&"spawner_ended"), "{kinds:?}");
            assert!(stderr.contains(FORMATION_CLOSED), "{stderr}");
            assert!(!stderr.contains(SPAWNER_CLOSED), "{stderr}");
        }
    }
}

// ── The variable is read strictly ─────────────────────────────────────────────

/// The highest descriptor a process may have, which nothing has open.
#[allow(unsafe_code)]
fn closed_descriptor() -> String {
    // SAFETY: `rlimit` is a zeroed plain-data struct `getrlimit` fills in.
    let limit = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur
    };
    limit.saturating_sub(1).min(i32::MAX as u64).to_string()
}

#[test]
fn an_unreadable_spawner_lifeline_refuses_before_any_session() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let project = child_project(&server);
    let mur = assert_cmd::cargo::cargo_bin("mur");
    let regular = project.path().join("regular-file");
    std::fs::write(&regular, "not a pipe").unwrap();
    let closed = closed_descriptor();
    let closed_reason = format!("descriptor {closed} is not open");

    // `exec mur … <redirection>` gives the session descriptor 9 as named.
    let cases: [(&str, &str, &str, &str); 6] = [
        (
            "a non-number",
            "three",
            "",
            "it is not a decimal descriptor number",
        ),
        (
            "below 3",
            "2",
            "",
            "descriptor 2 is a standard stream, and a lifeline is 3 or above",
        ),
        ("a closed descriptor", &closed, "", &closed_reason),
        (
            "a character device",
            "9",
            "9</dev/null",
            "descriptor 9 is a character device, and a lifeline is a pipe's read end",
        ),
        (
            "a regular file",
            "9",
            "9<\"$REGULAR\"",
            "descriptor 9 is a regular file, and a lifeline is a pipe's read end",
        ),
        (
            "a pipe's write end",
            "9",
            "9>&1",
            "descriptor 9 is open for writing, and a child holds only a read end",
        ),
    ];
    for (case, value, redirect, reason) in cases {
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "exec \"$MUR\" run --manifest \"$MANIFEST\" --json {redirect}"
            ))
            .env("MUR", &mur)
            .env("MANIFEST", project.path().join("murmur.yaml"))
            .env("REGULAR", &regular)
            .env(SPAWNER_LIFELINE_ENV, value)
            .current_dir(project.path())
            .env("HOME", home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(FORMATION_ID_ENV)
            .env_remove(FORMATION_LIFELINE_ENV)
            .env_remove(SPAWNER_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(output.status.code(), Some(0), "{case}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "error[E-RUN-020]: MURMUR_SPAWNER_LIFELINE does not carry this session's spawner \
                 lifeline: {reason}; a delegated child that cannot hear its spawner would keep \
                 running after the session that delegated to it has ended, so the launch is refused"
            )),
            "{case}: {stderr}"
        );
        assert!(
            stderr.contains(
                "MURMUR_SPAWNER_LIFELINE is set by a parent capsule's runtime for each child it \
                 launches, beside MURMUR_SPAWNER, and is not for operators; unset it to run this \
                 capsule directly"
            ),
            "{case}: {stderr}"
        );
        assert!(
            sessions_under(project.path()).is_empty(),
            "{case}: a refused launch left a session"
        );
    }

    // Blank is absent: the launch is not refused.
    let blank = command(home.path(), project.path(), &["--explain-scope"])
        .env(SPAWNER_LIFELINE_ENV, "  ")
        .output()
        .unwrap();
    assert_eq!(
        blank.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&blank.stderr)
    );
}

// ── A script capsule ──────────────────────────────────────────────────────────

/// A script capsule whose run lasts eight seconds and then writes `out/done.txt`.
fn script_project() -> TempDir {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("murmur.yaml"),
        "name: sleepy-script\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::copy(
        common::fixture_path("run/components/capsule-sleep-long.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    project
}

/// Start the script capsule with a spawner lifeline, its standard error closed so its diagnostics
/// land in `logs/bootstrap.log`.
fn start_script(home: &Path, project: &Path) -> (Child, ChildLifeline) {
    let mut command = command(home, project, &["--task", "sleep a while"]);
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut lifeline = ChildLifeline::new().unwrap();
    lifeline.hand_to(&mut command);
    let mut child = command.spawn().unwrap();
    lifeline.spawned();
    drop(child.stderr.take());
    (child, lifeline)
}

/// Whether the process `pid` has a thread named `name`, as the kernel keeps it: its first 15
/// bytes. A thread's name is readable in `/proc` even for a process that made itself
/// non-dumpable, as `mur` does.
fn has_thread(pid: u32, name: &str) -> bool {
    let kept = &name[..name.len().min(15)];
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == kept)
        })
}

fn wait_with_limit(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A script capsule is not refused for holding a spawner lifeline. EOF mid-run ends the process
/// with status 143, what a `SIGTERM` does to it, and says why in `logs/bootstrap.log`; left alone
/// it runs to completion.
#[test]
fn a_script_capsule_child_ends_its_run_when_its_spawner_lifeline_closes() {
    let home = driver_home();

    let project = script_project();
    let (mut child, mut lifeline) = start_script(home.path(), project.path());
    // The watcher starts as the launch reaches the run, which then sleeps for eight seconds.
    let deadline = Instant::now() + READY_LIMIT;
    while !has_thread(child.id(), "spawner-lifeline") {
        assert!(child.try_wait().unwrap().is_none(), "the run ended early");
        assert!(Instant::now() < deadline, "the lifeline was never watched");
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "the run ended early");
    let closed = Instant::now();
    lifeline.close();
    let status = wait_with_limit(&mut child, Duration::from_secs(5)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("the script capsule was still running 5s after EOF")
    });
    eprintln!(
        "[measure] script capsule: EOF to exit {} ms",
        closed.elapsed().as_millis()
    );
    assert_eq!(status.code(), Some(143), "{status}");
    assert!(common::find_file(project.path(), "done.txt").is_none());
    let sessions = sessions_under(project.path());
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let log =
        std::fs::read_to_string(sessions[0].join("logs").join("bootstrap.log")).unwrap_or_default();
    assert!(log.contains(SPAWNER_CLOSED), "{log}");
    assert!(log.contains("ending the run"), "{log}");

    let project = script_project();
    let (mut child, lifeline) = start_script(home.path(), project.path());
    let status = wait_with_limit(&mut child, Duration::from_secs(120)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("the script capsule never finished")
    });
    assert!(status.success(), "{status}");
    assert!(common::find_file(project.path(), "done.txt").is_some());
    drop(lifeline);
}
