//! A formation member's lifeline, at the binary: one `mur run` member started directly, with a
//! formation id and a lifeline whose write end this test holds through [`MemberLifeline`], against
//! a scripted model.
//!
//! EOF on the lifeline winds the member down as a first `SIGTERM` would, writing `formation_ended`
//! first; bytes on it are not EOF; a value that is not this member's read end refuses the launch;
//! a member with a formation id and no lifeline warns `W-RUN-007` and runs until it is stopped.

#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use capsule_runtime::formation::{FORMATION_ID_ENV, FORMATION_PEERS_ENV};
use capsule_runtime::{FormationId, MemberLifeline, FORMATION_LIFELINE_ENV};
use common::door_capsule::{
    agent_project, driver_home, end_turn, message, rpc, wait_for_requests, AUTHENTICATION_YAML,
    QUEUE_SLEEP_YAML,
};
use common::{
    assert_wound_down_by_formation, event_kinds as kinds, read_whole_trace as read_trace,
    sessions_under, signal, ScriptedServer,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// How long a member has to exit once its lifeline is closed: the runtime's own teardown
/// deadline, with room for a loaded host.
const EXIT_LIMIT: Duration = Duration::from_secs(25);

/// How long a member has to print its readiness line.
const READY_LIMIT: Duration = Duration::from_secs(120);

const LIFELINE_CLOSED: &str = "formation lifeline closed";

/// A queue/sleep agent with an authenticated door, against `server`.
fn member_project(server: &ScriptedServer) -> TempDir {
    agent_project(
        &server.endpoint,
        "coder",
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    )
}

/// `mur run --manifest <project>/murmur.yaml --json <args>` under `home`, with no formation
/// variable, door token or provider key inherited from this process.
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
        .env_remove("MURMUR_SPAWNER");
    command
}

/// A member started by this test, read as it runs.
struct Member {
    child: Child,
    startup: Value,
    stderr: Arc<Mutex<String>>,
    /// The write end this test holds, when the member was handed a lifeline.
    lifeline: Option<MemberLifeline>,
    _project: TempDir,
}

impl Member {
    /// Start a member in `formation` (when given), handed a lifeline when `lifeline`, and wait for
    /// its readiness line.
    fn start(
        home: &Path,
        server: &ScriptedServer,
        formation: Option<&FormationId>,
        lifeline: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let project = member_project(server);
        let mut command = command(home, project.path(), &[]);
        command
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(formation) = formation {
            command.env(FORMATION_ID_ENV, formation.as_str());
        }
        let mut lifeline = lifeline.then(|| MemberLifeline::new().unwrap());
        if let Some(lifeline) = &lifeline {
            lifeline.hand_to(&mut command);
        }
        let mut child = command.spawn().unwrap();
        if let Some(lifeline) = &mut lifeline {
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
                eprintln!("[member] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        let line = lines.recv_timeout(READY_LIMIT).unwrap_or_else(|_| {
            panic!(
                "the member printed no readiness line; stderr:\n{}",
                stderr.lock().unwrap()
            )
        });
        Self {
            child,
            startup: serde_json::from_str(&line).unwrap(),
            stderr,
            lifeline,
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

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn session_dir(&self) -> PathBuf {
        PathBuf::from(self.startup["workdir"].as_str().unwrap())
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Close the write end: the member reads EOF.
    fn close_lifeline(&mut self) {
        self.lifeline
            .as_mut()
            .expect("the member was handed a lifeline")
            .close();
    }

    /// The exit status, once the member exits within `limit`.
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
                "the member was still running {limit:?} later; stderr:\n{}",
                self.stderr()
            )
        })
    }

    /// Every record of the member's `trace.jsonl`, each line of which parses and none of which is
    /// a torn tail.
    fn trace(&self) -> Vec<Value> {
        read_trace(&self.session_dir().join("trace.jsonl"))
    }

    /// Ask the door who it is, under the operator token.
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
}

impl Drop for Member {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn position(events: &[Value], kind: &str) -> Option<usize> {
    events.iter().position(|event| event["event_type"] == kind)
}

// ── A member winds down on EOF ────────────────────────────────────────────────

#[test]
fn an_idle_member_winds_down_when_its_lifeline_closes() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let formation = FormationId::mint();
    let mut member = Member::start(home.path(), &server, Some(&formation), true, &[]);
    assert_eq!(member.startup["formation_id"], formation.as_str());

    member.close_lifeline();
    member.assert_exits_within(EXIT_LIMIT);
    let events = member.trace();
    assert_wound_down_by_formation(&events, formation.as_str());
    let tail = kinds(&events);
    assert_eq!(&tail[tail.len() - 2..], ["formation_ended", "session_end"]);
    let stderr = member.stderr();
    assert!(stderr.contains(LIFELINE_CLOSED), "{stderr}");
    assert!(!stderr.contains("SIGTERM received"), "{stderr}");
    assert!(!stderr.contains("W-RUN-007"), "{stderr}");
}

#[test]
fn a_member_mid_task_cancels_it_when_its_lifeline_closes() {
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let formation = FormationId::mint();
    // The task-end delay (debug builds only) holds the door open long enough to ask it.
    let mut member = Member::start(
        home.path(),
        &server,
        Some(&formation),
        true,
        &[("MURMUR_DEBUG_TASK_END_DELAY_MS", "2000")],
    );
    let task_id = member.send_task();
    wait_for_requests(&server, 1);

    member.close_lifeline();
    if cfg!(debug_assertions) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let got = rpc(
                &member.url(),
                Some(&member.token()),
                "tasks/get",
                json!({ "id": task_id }),
            );
            let state = got.json()["result"]["status"]["state"].clone();
            if state == "canceled" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the caller never saw the task canceled: {got:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
    member.assert_exits_within(EXIT_LIMIT);
    let events = member.trace();
    assert_wound_down_by_formation(&events, formation.as_str());
    let ended = position(&events, "formation_ended").unwrap();
    let canceled = events
        .iter()
        .position(|event| event["event_type"] == "task_canceled" && event["task_id"] == task_id)
        .unwrap_or_else(|| panic!("no task_canceled: {:?}", kinds(&events)));
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

#[test]
fn bytes_on_a_lifeline_are_not_its_end() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let formation = FormationId::mint();
    let mut member = Member::start(home.path(), &server, Some(&formation), true, &[]);
    let session_id = member.session_id();

    let write = member.lifeline.as_ref().unwrap().write_fd().unwrap();
    let bytes = [b'x'; 64];
    // SAFETY: writes from a live 64-byte array to the write end this test holds.
    #[allow(unsafe_code)]
    let written = unsafe { libc::write(write, bytes.as_ptr().cast(), bytes.len()) };
    assert_eq!(written, 64);
    let held = Instant::now();
    while held.elapsed() < Duration::from_secs(2) {
        assert!(member.child.try_wait().unwrap().is_none(), "bytes ended it");
        assert!(member.door_session_id().contains(&session_id));
        thread::sleep(Duration::from_millis(250));
    }

    member.close_lifeline();
    member.assert_exits_within(EXIT_LIMIT);
    assert_wound_down_by_formation(&member.trace(), formation.as_str());
}

// ── The variable is read strictly ─────────────────────────────────────────────

#[test]
fn an_unreadable_lifeline_refuses_before_any_session() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let project = member_project(&server);
    let formation = FormationId::mint();
    let mur = assert_cmd::cargo::cargo_bin("mur");
    let regular = project.path().join("regular-file");
    std::fs::write(&regular, "not a pipe").unwrap();

    // `exec mur … <redirection>` gives the member descriptor 9 as named; `$LIFELINE` is what the
    // variable says.
    let cases: [(&str, Option<&str>, &str, &str); 7] = [
        (
            "without a formation id",
            None,
            "",
            "MURMUR_FORMATION_ID is not",
        ),
        (
            "a non-number",
            Some(""),
            "",
            "not a decimal descriptor number",
        ),
        ("below 3", Some(""), "", "standard stream"),
        ("a closed descriptor", Some(""), "", "is not open"),
        (
            "a character device",
            Some(""),
            "9</dev/null",
            "a character device",
        ),
        ("a pipe's write end", Some(""), "9>&1", "open for writing"),
        (
            "a regular file",
            Some(""),
            "9<\"$REGULAR\"",
            "a regular file",
        ),
    ];
    for (index, (case, formation_set, redirect, expected)) in cases.into_iter().enumerate() {
        let value = match index {
            1 => "nine",
            2 => "2",
            3 => "987",
            _ => "9",
        };
        let mut lifeline = MemberLifeline::new().unwrap();
        let mut sh = Command::new("/bin/sh");
        sh.arg("-c")
            .arg(format!(
                "exec \"$MUR\" run --manifest \"$MANIFEST\" --json {redirect}"
            ))
            .env("MUR", &mur)
            .env("MANIFEST", project.path().join("murmur.yaml"))
            .env("REGULAR", &regular)
            .current_dir(project.path())
            .env("HOME", home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(FORMATION_ID_ENV)
            .env_remove(FORMATION_PEERS_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if formation_set.is_some() {
            sh.env(FORMATION_ID_ENV, formation.as_str());
        }
        if index == 0 {
            // A real read end, handed over properly, without the id it belongs with.
            lifeline.hand_to(&mut sh);
        } else {
            sh.env(FORMATION_LIFELINE_ENV, value);
        }
        let output = sh.output().unwrap();
        lifeline.spawned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{case}: {stderr}");
        assert!(stderr.contains("error[E-RUN-046]"), "{case}: {stderr}");
        assert!(stderr.contains(FORMATION_LIFELINE_ENV), "{case}: {stderr}");
        assert!(stderr.contains(expected), "{case}: {stderr}");
        assert!(
            sessions_under(project.path()).is_empty(),
            "{case}: a refused launch left a session"
        );
    }

    // Blank is absent, as a blank formation id is: the launch is not refused.
    let blank = command(home.path(), project.path(), &["--explain-scope"])
        .env(FORMATION_LIFELINE_ENV, "  ")
        .output()
        .unwrap();
    assert_eq!(
        blank.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&blank.stderr)
    );
}

// ── A member started by hand ──────────────────────────────────────────────────

fn warnings(stderr: &str) -> usize {
    stderr.matches("warning[W-RUN-007]").count()
}

#[test]
fn a_member_started_by_hand_warns_once_and_runs_until_stopped() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());
    let formation = FormationId::mint();
    let mut member = Member::start(home.path(), &server, Some(&formation), false, &[]);
    let session_id = member.session_id();
    assert!(member.door_session_id().contains(&session_id));

    thread::sleep(Duration::from_secs(30));
    assert!(
        member.child.try_wait().unwrap().is_none(),
        "the member did not keep running"
    );
    let stderr = member.stderr();
    assert_eq!(warnings(&stderr), 1, "{stderr}");
    let warning = stderr
        .lines()
        .find(|line| line.contains("warning[W-RUN-007]"))
        .unwrap();
    assert!(warning.contains(formation.as_str()), "{warning}");
    assert!(warning.contains("mur stop"), "{warning}");

    let stopped = common::door_capsule::mur(home.path(), &[])
        .args(["stop", &session_id])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    member.assert_exits_within(EXIT_LIMIT);
    let events = member.trace();
    assert!(
        position(&events, "formation_ended").is_none(),
        "{:?}",
        kinds(&events)
    );
    assert_eq!(kinds(&events).last(), Some(&"session_end"));
    assert!(member.stderr().contains("SIGTERM received"));
}

#[test]
fn no_warning_without_a_formation_or_for_a_delegated_child() {
    let home = driver_home();
    let server = ScriptedServer::start(Vec::new());

    let mut plain = Member::start(home.path(), &server, None, false, &[]);
    signal(plain.pid(), libc::SIGTERM);
    plain.assert_exits_within(EXIT_LIMIT);
    assert_eq!(warnings(&plain.stderr()), 0, "{}", plain.stderr());

    // A delegated child is started with `--spawn-grant-stdin` and its parent's formation id, and
    // no lifeline. The warning would be printed before staging, whatever the launch does next.
    let project = member_project(&server);
    let formation = FormationId::mint();
    let mut child = command(home.path(), project.path(), &["--spawn-grant-stdin"])
        .env(FORMATION_ID_ENV, formation.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "grant").unwrap();
    }
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() && started.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(warnings(&stderr), 0, "{stderr}");
}

// ── Termination is begun once ─────────────────────────────────────────────────

/// A member whose in-flight task is slow to record its end once cancelled, so its teardown lasts
/// long enough to signal it during.
fn member_with_a_slow_teardown(
    home: &TempDir,
    server: &ScriptedServer,
    delay_ms: &str,
) -> (Member, FormationId) {
    let formation = FormationId::mint();
    let member = Member::start(
        home.path(),
        server,
        Some(&formation),
        true,
        &[("MURMUR_DEBUG_TASK_END_DELAY_MS", delay_ms)],
    );
    member.send_task();
    wait_for_requests(server, 1);
    (member, formation)
}

fn skip_without_the_delay_seam(name: &str) -> bool {
    if !cfg!(debug_assertions) {
        eprintln!("[SKIP] {name}: the task-end delay seam exists only in debug builds");
        return true;
    }
    false
}

#[test]
fn eof_then_one_sigterm_still_ends_through_the_teardown() {
    if skip_without_the_delay_seam("eof_then_one_sigterm_still_ends_through_the_teardown") {
        return;
    }
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let (mut member, formation) = member_with_a_slow_teardown(&home, &server, "2000");
    member.close_lifeline();
    thread::sleep(Duration::from_millis(300));
    signal(member.pid(), libc::SIGTERM);
    member.assert_exits_within(EXIT_LIMIT);
    let events = member.trace();
    assert_wound_down_by_formation(&events, formation.as_str());
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "task_end" && event["exit_status"] == "canceled"),
        "the teardown was cut short: {:?}",
        kinds(&events)
    );
}

#[test]
fn sigterm_then_eof_writes_no_formation_ended() {
    if skip_without_the_delay_seam("sigterm_then_eof_writes_no_formation_ended") {
        return;
    }
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let (mut member, _) = member_with_a_slow_teardown(&home, &server, "2000");
    signal(member.pid(), libc::SIGTERM);
    thread::sleep(Duration::from_millis(300));
    member.close_lifeline();
    member.assert_exits_within(EXIT_LIMIT);
    let events = member.trace();
    assert!(
        position(&events, "formation_ended").is_none(),
        "{:?}",
        kinds(&events)
    );
    assert_eq!(
        kinds(&events).last(),
        Some(&"session_end"),
        "{:?}",
        kinds(&events)
    );
    let stderr = member.stderr();
    assert!(stderr.contains("SIGTERM received"), "{stderr}");
    assert!(!stderr.contains(LIFELINE_CLOSED), "{stderr}");
}

#[test]
fn eof_then_two_sigterms_exits_at_the_second() {
    if skip_without_the_delay_seam("eof_then_two_sigterms_exits_at_the_second") {
        return;
    }
    let home = driver_home();
    let server =
        ScriptedServer::start_with_delay(vec![end_turn(1, "late")], Duration::from_secs(120));
    let (mut member, _) = member_with_a_slow_teardown(&home, &server, "30000");
    member.close_lifeline();
    thread::sleep(Duration::from_millis(300));
    signal(member.pid(), libc::SIGTERM);
    thread::sleep(Duration::from_millis(500));
    assert!(
        member.child.try_wait().unwrap().is_none(),
        "the first SIGTERM after EOF ended the process at once"
    );
    signal(member.pid(), libc::SIGTERM);
    let status = member
        .exited_within(Duration::from_secs(2))
        .expect("the member was still running 2 seconds after the second SIGTERM");
    assert_eq!(status.code(), Some(143));
}
