//! `mur run --roster`: a declared formation launched end to end, every member a real `mur run`
//! process against a scripted model, and torn down when the entry member's task ends.
//!
//! Every member carries the test's unique project path on its command line through `--workdir`,
//! which is how "no member process remains" is checked: no process on the host has that path on
//! its `/proc/<pid>/cmdline`, and every pid the launcher reported is dead.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use common::door_capsule::{driver_home, rpc, DRIVER_NAME, DRIVER_VERSION};
use common::{publish_to_store, tool_result_text, tool_use_response, ScriptedServer};
use murmur_artifact::LocalRegistry;
use serde_json::{json, Value};
use tempfile::TempDir;

const PROBE_TOOL: &str = "formation-probe";
const PROBE_VERSION: &str = "0.1.0";
const PROBE_CALL: &str = "toolu_formation_probe";

/// How long one launch may take end to end before the test gives up on it.
const LAUNCH_LIMIT: Duration = Duration::from_secs(240);

/// Formation launches start three or four `mur run` processes each; running several suites of
/// them at once on one host turns a readiness deadline into a measure of contention.
fn launch_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Artifacts ─────────────────────────────────────────────────────────────────

/// An agent capsule's manifest body: its driver against `endpoint`, an authenticated door that
/// serves peers, `lifecycle`, and `extra_artifacts` / `network_allow` for the entry member.
fn member_manifest(
    endpoint: &str,
    lifecycle: &str,
    extra_artifacts: &str,
    network_allow: &str,
    driver_version: &str,
) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER_NAME}\n    version: {driver_version}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n{extra_artifacts}\
         capabilities:\n  network:\n    allow: [{network_allow}]\n\
         lifecycle:\n  {lifecycle}\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
         exports:\n  peer_tasks:\n    accept: true\n\
         network:\n  authentication:\n    scheme: bearer\n"
    )
}

const PEER_LIFECYCLE: &str = "task_acceptance: queue\n  after_task: sleep";

fn end_turn_response(text: &str) -> String {
    common::door_capsule::end_turn(1, text)
}

/// A model that calls `formation-probe` once and then ends its turn, holding the second reply
/// until `release` is sent — so a case can look at a running formation before its task ends.
fn probe_model() -> (ScriptedServer, mpsc::Sender<()>, Arc<AtomicUsize>) {
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let arrived = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&arrived);
    let server = ScriptedServer::start_answering(2, move |_| {
        let calls = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if calls == 1 {
            tool_use_response(PROBE_CALL, PROBE_TOOL, json!({}))
        } else {
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(120));
            end_turn_response("probed")
        }
    });
    (server, release, arrived)
}

// ── A project holding a roster ────────────────────────────────────────────────

/// One member of a fixture roster.
struct Member {
    name: &'static str,
    version: &'static str,
    entry: bool,
    /// A driver version nothing installed, so the member cannot start.
    broken: bool,
}

const fn peer(name: &'static str, version: &'static str) -> Member {
    Member {
        name,
        version,
        entry: false,
        broken: false,
    }
}

const fn entry(name: &'static str, version: &'static str) -> Member {
    Member {
        name,
        version,
        entry: true,
        broken: false,
    }
}

const fn broken(name: &'static str, version: &'static str) -> Member {
    Member {
        name,
        version,
        entry: false,
        broken: true,
    }
}

/// The scenario-1 roster's members.
const CODER: Member = peer("coder", "1.2.0");
const REVIEWER: Member = peer("reviewer", "0.9.0");
const PLANNER: Member = entry("planner", "0.3.0");

/// A project directory with every member published into its store, a `roster.yaml`, and a scratch
/// `HOME` holding the driver.
struct Project {
    dir: TempDir,
    home: TempDir,
    model: ScriptedServer,
    release: mpsc::Sender<()>,
    /// How many requests the entry member's model has received, counted on arrival: the second
    /// is the one held until `release`.
    arrived: Arc<AtomicUsize>,
    /// Kept so the peers' gateway endpoint stays an address nothing else is handed.
    _peer_model: ScriptedServer,
}

impl Project {
    fn new(members: &[Member], reachability: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("formation-launch-")
            .tempdir()
            .unwrap();
        let home = driver_home();
        let (model, release, arrived) = probe_model();
        let peer_model = ScriptedServer::start(Vec::new());
        let store = dir.path().join(".murmur").join("artifacts");
        publish_to_store(
            &store,
            PROBE_TOOL,
            PROBE_VERSION,
            "tool",
            "runtime: tool\n",
            Some((
                "tool.wasm",
                &common::fixture_path("formation-probe/tool/formation-probe.wasm"),
            )),
        );
        let mut roster = String::from("roster_version: 1\nmembers:\n");
        for member in members {
            let driver_version = if member.broken {
                "9.9.9"
            } else {
                DRIVER_VERSION
            };
            let body = if member.entry {
                member_manifest(
                    &model.endpoint,
                    "task_acceptance: single\n  after_task: exit",
                    &format!(
                        "  - name: {PROBE_TOOL}\n    version: {PROBE_VERSION}\n    runtime: tool\n"
                    ),
                    "localhost",
                    driver_version,
                )
            } else {
                member_manifest(&peer_model.endpoint, PEER_LIFECYCLE, "", "", driver_version)
            };
            publish_to_store(&store, member.name, member.version, "capsule", &body, None);
            roster.push_str(&format!(
                "  - name: {name}\n    capsule: {name}\n    version: {version}\n{entry}",
                name = member.name,
                version = member.version,
                entry = if member.entry {
                    "    entry: true\n"
                } else {
                    ""
                },
            ));
        }
        roster.push_str(reachability);
        std::fs::write(dir.path().join("roster.yaml"), roster).unwrap();
        Self {
            dir,
            home,
            model,
            release,
            arrived,
            _peer_model: peer_model,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Block until the entry member is mid-task: its model holds the reply to the probe's result.
    fn await_entry_mid_task(&self) {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        while self.arrived.load(Ordering::SeqCst) < 2 {
            assert!(
                Instant::now() < deadline,
                "the entry member never reached its second turn"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `mur <args>` in the project directory, under the scratch `HOME`, with no formation, door
    /// token or provider key inherited from this process.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(args)
            .current_dir(self.path())
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove("MURMUR_SPAWNER");
        command
    }

    /// `mur <args>` run to completion.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Finished {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        let output = command.output().unwrap();
        Finished {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// `mur run --roster --json --task probe <extra>`, started and read as it runs.
    fn launch(&self, extra: &[&str], env: &[(&str, &str)]) -> Launcher {
        let mut args = vec!["run", "--roster", "--json", "--task", "probe"];
        args.extend_from_slice(extra);
        let mut command = self.command(&args);
        command
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        let (line_tx, lines) = mpsc::channel::<(Instant, String)>();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stdout);
        let out = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                seen.lock().unwrap().push(line.clone());
                let _ = line_tx.send((Instant::now(), line));
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let err = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[launcher] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        Launcher {
            child,
            started,
            lines,
            stdout,
            stderr,
        }
    }

    /// Every `ses_*` session directory members created under the project.
    fn sessions(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.path().join(".murmur")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ses_"))
            })
            .collect()
    }

    /// The session directory whose trace names `capsule` as the capsule it ran.
    fn sessions_of(&self, capsule: &str) -> Vec<PathBuf> {
        self.sessions()
            .into_iter()
            .filter(|session| {
                std::fs::read_to_string(session.join("trace.jsonl"))
                    .ok()
                    .and_then(|trace| trace.lines().next().map(str::to_string))
                    .and_then(|line| serde_json::from_str::<Value>(&line).ok())
                    .is_some_and(|start| start["capsule_name"] == capsule)
            })
            .collect()
    }

    /// The operator token the running record of `session_id` holds.
    fn door_token(&self, session_id: &str) -> String {
        let record = self
            .home
            .path()
            .join(".murmur")
            .join("running")
            .join(format!("{session_id}.json"));
        let record: Value =
            serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
        record["door_token"].as_str().unwrap().to_string()
    }

    /// Fail unless no process on the host carries this project's path on its command line within
    /// `limit`.
    fn assert_no_member_remains(&self, pids: &[u32], limit: Duration) {
        let needle = self.path().to_string_lossy().into_owned();
        let deadline = Instant::now() + limit;
        loop {
            let holders = processes_mentioning(&needle);
            let alive: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
            if holders.is_empty() && alive.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "member processes remain after {limit:?}: by command line {holders:?}, by \
                     reported pid {alive:?}"
                );
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// A running `mur run --roster`.
struct Launcher {
    child: Child,
    started: Instant,
    lines: mpsc::Receiver<(Instant, String)>,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
}

impl Launcher {
    /// The next stdout line, parsed, and when it arrived.
    fn next_json(&self) -> (Instant, Value) {
        let (at, line) = self
            .lines
            .recv_timeout(LAUNCH_LIMIT)
            .unwrap_or_else(|_| panic!("no stdout line; stderr:\n{}", self.stderr()));
        let value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdout line is not JSON: {line}"));
        (at, value)
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("the launcher did not exit; stderr:\n{}", self.stderr());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn stdout(&self) -> Vec<String> {
        // The relay thread may still be appending the last line when the process has exited.
        thread::sleep(Duration::from_millis(200));
        self.stdout.lock().unwrap().clone()
    }

    fn signal(&self, signal: i32) {
        signal_pid(self.child.id(), signal);
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.signal(libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(60);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[allow(unsafe_code)]
fn signal_pid(pid: u32, signal: i32) {
    // SAFETY: `kill` takes two integers and dereferences nothing; `pid` is a child of this test.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// Whether `pid` is a live process: present, and not a zombie.
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                != Some("Z")
        }
        Err(_) => false,
    }
}

/// Every live process whose command line contains `needle`.
fn processes_mentioning(needle: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains(needle) && alive(pid) {
            found.push((pid, cmdline));
        }
    }
    found
}

/// Every pid a formation line and a readiness line reported.
fn reported_pids(formation: &Value, entry: Option<&Value>) -> Vec<u32> {
    let mut pids: Vec<u32> = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| peer["pid"].as_u64().unwrap() as u32)
        .collect();
    if let Some(entry) = entry {
        pids.push(entry["pid"].as_u64().unwrap() as u32);
    }
    pids
}

/// `stdout` and `stderr` with the entry member's own readiness-line tokens taken out: the one
/// token the launcher's output may carry is the entry member's, on its own readiness line.
fn assert_no_token_but_the_entrys(stdout: &[String], stderr: &str) {
    for (index, line) in stdout.iter().enumerate() {
        let mut value: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        if index == 1 {
            value.as_object_mut().map(|object| object.remove("tokens"));
        }
        let text = if value.is_null() {
            line.clone()
        } else {
            value.to_string()
        };
        assert!(
            !text.contains("mdt1."),
            "stdout line {index} carries a token: {line}"
        );
    }
    assert!(!stderr.contains("mdt1."), "stderr carries a token");
}

fn assert_formation_line(formation: &Value, entry: &str, peers: &[&str]) {
    assert_eq!(formation["entry"], entry, "{formation}");
    let id = formation["formation_id"].as_str().unwrap();
    assert!(
        id.len() == 36
            && id.starts_with("frm_")
            && id[4..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{id}"
    );
    let names: Vec<&str> = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| peer["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, peers, "{formation}");
    for peer in formation["peers"].as_array().unwrap() {
        let keys: Vec<&String> = peer.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["name", "pid", "session_id", "url"], "{peer}");
        assert!(peer["session_id"].as_str().unwrap().starts_with("ses_"));
        assert!(peer["url"].as_str().unwrap().starts_with("http://"));
    }
    let keys: Vec<&String> = formation.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["entry", "formation_id", "peers"], "{formation}");
}

/// Every line of every peer's `trace.jsonl` parses: none was torn by the teardown. The entry
/// member's session, `entry_session`, is skipped: a case that `SIGKILL`s it may tear its own.
fn assert_traces_intact(project: &Project, entry_session: &str) {
    let sessions: Vec<PathBuf> = project
        .sessions()
        .into_iter()
        .filter(|session| !session.ends_with(entry_session))
        .collect();
    assert_eq!(sessions.len(), 2, "one session per peer: {sessions:?}");
    for session in sessions {
        let trace = std::fs::read_to_string(session.join("trace.jsonl")).unwrap_or_default();
        for (number, line) in trace.lines().enumerate() {
            assert!(
                serde_json::from_str::<Value>(line).is_ok(),
                "{}: line {} does not parse: {line}",
                session.display(),
                number + 1
            );
        }
    }
}

const FULL_REACH: &str = "reachability:\n  - from: planner\n    to: [coder, reviewer]\n";

// ── Scenarios ─────────────────────────────────────────────────────────────────

/// Scenario 1: the formation comes up, the entry member reaches each peer's door, and the whole
/// formation is gone once the entry member's task ends.
#[test]
fn a_declared_formation_launches_reaches_its_peers_and_goes() {
    let _lock = launch_lock();
    let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
    let mut launcher = project.launch(&[], &[]);

    let (_, formation) = launcher.next_json();
    assert_formation_line(&formation, "planner", &["coder", "reviewer"]);
    let (_, planner) = launcher.next_json();
    assert_eq!(
        planner["formation_id"], formation["formation_id"],
        "{planner}"
    );
    assert_eq!(planner["name"], "planner");

    // While up, each peer's door answers as the session the formation line names.
    for peer in formation["peers"].as_array().unwrap() {
        let session_id = peer["session_id"].as_str().unwrap();
        let addr = peer["url"].as_str().unwrap().trim_start_matches("http://");
        let token = project.door_token(session_id);
        let card = rpc(
            addr,
            Some(&token),
            "agent/getAuthenticatedExtendedCard",
            json!({}),
        );
        assert_eq!(card.status, 200, "{}", card.body);
        assert!(card.body.contains(session_id), "{}", card.body);
    }

    project.release.send(()).unwrap();
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let result = tool_result_text(&project.model.requests(), PROBE_CALL)
        .expect("the probe's result reached the model");
    assert_eq!(
        result.lines().collect::<Vec<_>>(),
        [
            "coder card=200 card_name=coder send=401",
            "reviewer card=200 card_name=reviewer send=401",
        ],
        "{result}"
    );

    let stdout = launcher.stdout();
    assert_eq!(stdout.len(), 2, "{stdout:?}");
    assert_no_token_but_the_entrys(&stdout, &launcher.stderr());
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
}

/// Every member's workdir is the project directory, where the entry member's task is written and
/// left behind. A peer takes work only at its door: a `task.md` already there — an earlier
/// launch's — is never run by a peer.
#[test]
fn a_task_file_in_the_project_is_never_a_peers_task() {
    let _lock = launch_lock();
    let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
    std::fs::write(project.path().join("task.md"), "an earlier launch's task").unwrap();
    let mut launcher = project.launch(&[], &[]);
    let (_, formation) = launcher.next_json();
    let (_, planner) = launcher.next_json();
    project.release.send(()).unwrap();
    assert_eq!(launcher.wait().code(), Some(0), "{}", launcher.stderr());

    for peer in ["coder", "reviewer"] {
        let sessions = project.sessions_of(peer);
        assert_eq!(sessions.len(), 1, "{peer}: {sessions:?}");
        let trace = std::fs::read_to_string(sessions[0].join("trace.jsonl")).unwrap();
        assert!(
            !trace.contains("\"task_start\""),
            "{peer} ran a task it was never sent: {trace}"
        );
    }
    let planner_trace =
        std::fs::read_to_string(project.sessions_of("planner")[0].join("trace.jsonl")).unwrap();
    assert!(planner_trace.contains("\"task_start\""), "{planner_trace}");
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
}

/// Scenario 2: only the members the entry member may call are handed to it; every member is
/// launched regardless.
#[test]
fn only_the_entry_members_callees_are_handed_to_it() {
    let _lock = launch_lock();
    for (reachability, expected) in [
        (
            "reachability:\n  - from: planner\n    to: [coder]\n",
            vec!["coder card=200 card_name=coder send=401"],
        ),
        ("", vec!["peers=absent"]),
    ] {
        let project = Project::new(&[CODER, REVIEWER, PLANNER], reachability);
        let mut launcher = project.launch(&[], &[]);
        let (_, formation) = launcher.next_json();
        assert_formation_line(&formation, "planner", &["coder", "reviewer"]);
        let (_, planner) = launcher.next_json();
        project.release.send(()).unwrap();
        assert_eq!(launcher.wait().code(), Some(0), "{}", launcher.stderr());

        let result = tool_result_text(&project.model.requests(), PROBE_CALL).unwrap();
        assert_eq!(
            result.lines().collect::<Vec<_>>(),
            expected,
            "{reachability}"
        );
        project.assert_no_member_remains(
            &reported_pids(&formation, Some(&planner)),
            Duration::from_secs(30),
        );
    }
}

/// Scenario 3: a peer that cannot start refuses the whole launch, leaves nothing running, and the
/// entry member never starts — wherever the broken member sits in the roster.
#[test]
fn a_peer_that_cannot_start_refuses_the_launch_and_leaves_nothing_running() {
    let _lock = launch_lock();
    for members in [
        [broken("broken", "0.1.0"), CODER, REVIEWER, PLANNER],
        [CODER, REVIEWER, PLANNER, broken("broken", "0.1.0")],
    ] {
        let project = Project::new(&members, FULL_REACH);
        let mut launcher = project.launch(&[], &[]);
        let status = launcher.wait();
        let stderr = launcher.stderr();
        assert_ne!(status.code(), Some(0), "{stderr}");
        assert!(launcher.stdout().is_empty(), "no formation line is printed");

        assert!(stderr.contains("error[E-RUN-045]"), "{stderr}");
        assert!(stderr.contains("'broken' (broken@0.1.0)"), "{stderr}");
        assert!(
            stderr.contains("exited with status 1 before reporting"),
            "{stderr}"
        );
        assert!(stderr.contains("its last stderr lines were:"), "{stderr}");
        assert!(stderr.contains("E-RUN-008"), "the tail is quoted: {stderr}");

        let stopped = stderr
            .lines()
            .find_map(|line| line.trim().strip_prefix("stopped: "))
            .unwrap_or_else(|| panic!("no stopped list: {stderr}"));
        let mut pids = Vec::new();
        for member in stopped.split(", ").filter(|member| *member != "none") {
            let (name, pid) = member.split_once(" (pid ").unwrap();
            assert!(["coder", "reviewer"].contains(&name), "{stopped}");
            pids.push(pid.trim_end_matches(')').parse::<u32>().unwrap());
        }
        for pid in &pids {
            assert!(!alive(*pid), "stopped member {pid} is alive");
        }
        assert!(
            project.sessions_of("planner").is_empty(),
            "planner never ran"
        );
        assert!(!stderr.contains("[planner]"), "{stderr}");
        assert!(
            project.model.requests().is_empty(),
            "planner never asked its model"
        );
        project.assert_no_member_remains(&pids, Duration::from_secs(30));
        assert!(!stderr.contains("mdt1."), "{stderr}");
    }
}

/// Scenario 5: a signal to the launcher, and the entry member's own death, both end the whole
/// formation with the documented status, and no member's trace is torn.
#[test]
fn the_entry_members_end_or_the_launchers_signal_ends_the_formation() {
    let _lock = launch_lock();
    for (kill_launcher, expected) in [(true, 143), (false, 137)] {
        let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
        let mut launcher = project.launch(&[], &[]);
        let (_, formation) = launcher.next_json();
        let (_, planner) = launcher.next_json();
        project.await_entry_mid_task();
        if kill_launcher {
            launcher.signal(libc::SIGTERM);
        } else {
            signal_pid(planner["pid"].as_u64().unwrap() as u32, libc::SIGKILL);
        }
        let status = launcher.wait();
        assert_eq!(
            status.code(),
            Some(expected),
            "stderr:\n{}",
            launcher.stderr()
        );
        let _ = project.release.send(());
        project.assert_no_member_remains(
            &reported_pids(&formation, Some(&planner)),
            Duration::from_secs(30),
        );
        assert_traces_intact(&project, planner["session_id"].as_str().unwrap());
    }
}

/// Scenario 6, at the binary: `MURMUR_FORMATION_PEERS` without a formation id, or unreadable, is
/// refused before any session exists.
#[test]
fn an_unreadable_or_orphaned_peers_variable_refuses_with_e_run_046() {
    let project = Project::new(&[CODER, PLANNER], "");
    let id = capsule_runtime::FormationId::mint();
    for env in [
        vec![(
            capsule_runtime::formation::FORMATION_PEERS_ENV,
            "coder=http://localhost:41873".to_string(),
        )],
        vec![
            (capsule_runtime::formation::FORMATION_ID_ENV, id.to_string()),
            (
                capsule_runtime::formation::FORMATION_PEERS_ENV,
                "coder=http://localhost:41873  reviewer=http://localhost:1".to_string(),
            ),
        ],
    ] {
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let run = project.run(
            &[
                "run",
                "--capsule",
                "planner",
                "--capsule-version",
                "0.3.0",
                "--json",
            ],
            &env,
        );
        assert_eq!(run.status.code(), Some(1), "{}", run.stderr);
        assert!(run.stderr.contains("error[E-RUN-046]"), "{}", run.stderr);
        assert!(run.stdout.is_empty(), "{}", run.stdout);
        assert!(
            project.sessions().is_empty(),
            "a refused launch left a session"
        );
    }
}

/// Scenario 7: a member runs the bytes admission bound it to, or refuses with `E-RUN-047`.
#[test]
fn a_member_whose_bytes_changed_since_admission_is_refused() {
    let _lock = launch_lock();
    let project = Project::new(&[CODER, PLANNER], "");
    let wrong = "0".repeat(64);
    let run = project.run(
        &[
            "run",
            "--capsule",
            "coder",
            "--capsule-version",
            "1.2.0",
            "--capsule-sha256",
            &wrong,
        ],
        &[],
    );
    assert_eq!(run.status.code(), Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("error[E-RUN-047]"), "{}", run.stderr);
    let installed = std::fs::read_to_string(
        LocalRegistry::new(project.path().join(".murmur").join("artifacts"))
            .sha256_path_for("coder", "1.2.0"),
    )
    .unwrap();
    assert!(run.stderr.contains(installed.trim()), "{}", run.stderr);
    assert!(run.stderr.contains(&wrong), "{}", run.stderr);
    assert!(project.sessions().is_empty(), "refused before staging");

    let help = project.run(&["run", "--help"], &[]);
    assert!(!help.stdout.contains("capsule-sha256"), "{}", help.stdout);

    // A reinstall between admission and the member's spawn: a stand-in `mur` swaps coder's bytes
    // for a different build of the same version, then runs the real binary.
    let alternate = TempDir::new().unwrap();
    publish_to_store(
        alternate.path(),
        "coder",
        "1.2.0",
        "capsule",
        &format!(
            "{}description: rebuilt\n",
            member_manifest(
                &project.model.endpoint,
                PEER_LIFECYCLE,
                "",
                "",
                DRIVER_VERSION
            )
        ),
        None,
    );
    let from = LocalRegistry::new(alternate.path());
    let to = LocalRegistry::new(project.path().join(".murmur").join("artifacts"));
    let wrapper = alternate.path().join("mur");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \"$*\" in *'--capsule coder '*) cp '{}' '{}'; cp '{}' '{}';; esac\n\
             exec '{}' \"$@\"\n",
            from.artifact_path_for("coder", "1.2.0").display(),
            to.artifact_path_for("coder", "1.2.0").display(),
            from.sha256_path_for("coder", "1.2.0").display(),
            to.sha256_path_for("coder", "1.2.0").display(),
            assert_cmd::cargo::cargo_bin("mur").display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut launcher = project.launch(&[], &[("MURMUR_MUR_BINARY", wrapper.to_str().unwrap())]);
    let status = launcher.wait();
    let stderr = launcher.stderr();
    assert_ne!(status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("error[E-RUN-045]"), "{stderr}");
    assert!(stderr.contains("'coder' (coder@1.2.0)"), "{stderr}");
    assert!(
        stderr.contains("E-RUN-047"),
        "the member's refusal is quoted: {stderr}"
    );
    assert!(launcher.stdout().is_empty());
    project.assert_no_member_remains(&[], Duration::from_secs(30));
}

/// Scenario 8: every refusal that needs no process is made before any process starts.
#[test]
fn refusals_before_anything_starts() {
    let project = Project::new(&[CODER, peer("planner", "0.3.0")], "");
    let launch = |args: &[&str], env: &[(&str, &str)]| {
        let mut full = vec!["run"];
        full.extend_from_slice(args);
        project.run(&full, env)
    };

    let no_entry = launch(&["--roster"], &[]);
    assert_eq!(no_entry.status.code(), Some(1));
    assert!(
        no_entry.stderr.contains("error[E-ROS-002]"),
        "{}",
        no_entry.stderr
    );

    let minted = capsule_runtime::FormationId::mint();
    let inside = launch(
        &["--roster"],
        &[(
            capsule_runtime::formation::FORMATION_ID_ENV,
            minted.as_str(),
        )],
    );
    assert_eq!(inside.status.code(), Some(1));
    assert!(
        inside.stderr.contains("error[E-RUN-046]"),
        "{}",
        inside.stderr
    );

    for conflicting in [
        &["--workdir", "x"][..],
        &["--bind", "0.0.0.0"],
        &["--capsule", "coder", "--capsule-version", "1.2.0"],
        &["--resume"],
        &["--context", "ctx"],
        &["--spawn-grant-stdin"],
        &["--system-prompt", "x"],
        &["--lifecycle-task-acceptance", "queue"],
        &["--lifecycle-after-task", "sleep"],
        &["--explain-scope"],
    ] {
        let mut args = vec!["--roster"];
        args.extend_from_slice(conflicting);
        let refused = launch(&args, &[]);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{conflicting:?}: {}",
            refused.stderr
        );
        assert!(
            refused.stderr.contains("cannot be used with"),
            "{conflicting:?}: {}",
            refused.stderr
        );
    }

    let empty = tempfile::tempdir().unwrap();
    let missing = launch(&["--roster", empty.path().to_str().unwrap()], &[]);
    assert!(
        missing.stderr.contains("error[E-ROS-001]"),
        "{}",
        missing.stderr
    );
    assert!(
        missing.stderr.contains("roster.yaml not found"),
        "{}",
        missing.stderr
    );

    let other = launch(&["--roster", "./other.yaml"], &[]);
    assert!(
        other.stderr.contains("error[E-ROS-001]"),
        "{}",
        other.stderr
    );
    assert!(
        other
            .stderr
            .contains("a roster is read from roster.yaml in its project directory"),
        "{}",
        other.stderr
    );

    assert!(project.sessions().is_empty(), "a refusal created a session");
    project.assert_no_member_remains(&[], Duration::from_secs(1));
}

/// Scenario 9: an edge between two peers is launched, and named once as having no address.
#[test]
fn a_peer_to_peer_edge_warns_once_and_launches() {
    let _lock = launch_lock();
    let project = Project::new(
        &[CODER, REVIEWER, PLANNER],
        "reachability:\n  - from: planner\n    to: [coder, reviewer]\n  - from: reviewer\n    to: [coder]\n",
    );
    let mut launcher = project.launch(&[], &[]);
    let (_, formation) = launcher.next_json();
    assert_formation_line(&formation, "planner", &["coder", "reviewer"]);
    let (_, planner) = launcher.next_json();
    project.release.send(()).unwrap();
    assert_eq!(launcher.wait().code(), Some(0), "{}", launcher.stderr());
    let stderr = launcher.stderr();
    assert_eq!(stderr.matches("warning[W-RUN-005]").count(), 1, "{stderr}");
    assert!(stderr.contains("reviewer \u{2192} coder"), "{stderr}");
    let result = tool_result_text(&project.model.requests(), PROBE_CALL).unwrap();
    assert_eq!(result.lines().count(), 2, "{result}");
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
}

/// Scenario 10: without `--roster`, `mur run` never reads `roster.yaml`, however broken.
#[test]
fn mur_run_without_roster_ignores_an_invalid_roster() {
    let project = Project::new(&[CODER, PLANNER], "");
    std::fs::write(project.path().join("roster.yaml"), "not: [a roster").unwrap();
    let run = project.run(
        &[
            "run",
            "--capsule",
            "coder",
            "--capsule-version",
            "1.2.0",
            "--explain-scope",
        ],
        &[],
    );
    assert_eq!(run.status.code(), Some(0), "{}", run.stderr);
    assert!(!run.stderr.contains("E-ROS"), "{}", run.stderr);
}

// ── The four-member cost ──────────────────────────────────────────────────────

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

/// Every descendant of `root`, `root` included.
fn process_tree(root: u32) -> Vec<u32> {
    let mut parents: Vec<(u32, u32)> = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) {
            if let Some(ppid) = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|ppid| ppid.parse::<u32>().ok())
            {
                parents.push((pid, ppid));
            }
        }
    }
    let mut tree = vec![root];
    let mut index = 0;
    while index < tree.len() {
        let parent = tree[index];
        tree.extend(
            parents
                .iter()
                .filter(|(_, ppid)| *ppid == parent)
                .map(|(pid, _)| *pid),
        );
        index += 1;
    }
    tree
}

/// Summed `Pss` of `pids`, in MB, and how many of them had no readable `smaps_rollup`.
fn pss_mb(pids: &[u32]) -> (f64, usize) {
    let rollups: Vec<Option<String>> = pids
        .iter()
        .map(|pid| std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok())
        .collect();
    let unreadable = rollups.iter().filter(|rollup| rollup.is_none()).count();
    let kb: u64 = rollups
        .iter()
        .flatten()
        .filter_map(|rollup| {
            rollup.lines().find_map(|line| {
                line.strip_prefix("Pss:").and_then(|rest| {
                    rest.trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
            })
        })
        .sum();
    (kb as f64 / 1024.0, unreadable)
}

/// `mur` makes itself non-dumpable, which hands its `smaps_rollup` to root. Preloaded into the
/// launcher, and inherited by every member, this turns that one `prctl` into a no-op so an
/// unprivileged run can read each process's `Pss`. The flag moves no memory.
const DUMPABLE_SHIM: &str = r#"
#define _GNU_SOURCE
#include <stdarg.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <unistd.h>

int prctl(int option, ...)
{
    va_list ap;
    va_start(ap, option);
    unsigned long a2 = va_arg(ap, unsigned long);
    unsigned long a3 = va_arg(ap, unsigned long);
    unsigned long a4 = va_arg(ap, unsigned long);
    unsigned long a5 = va_arg(ap, unsigned long);
    va_end(ap);
    if (option == PR_SET_DUMPABLE && a2 == 0)
        return 0;
    return (int)syscall(SYS_prctl, option, a2, a3, a4, a5);
}
"#;

/// [`DUMPABLE_SHIM`], built with the host's `cc` into `dir`.
fn build_dumpable_shim(dir: &Path) -> PathBuf {
    let source = dir.join("dumpable-shim.c");
    let library = dir.join("dumpable-shim.so");
    std::fs::write(&source, DUMPABLE_SHIM).unwrap();
    let built = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&library)
        .arg(&source)
        .status()
        .expect("cc builds the dumpable shim");
    assert!(built.success(), "cc failed to build the dumpable shim");
    library
}

#[allow(unsafe_code)]
fn children_cpu_seconds() -> f64 {
    // SAFETY: `getrusage` writes one `rusage` into the zeroed struct it is handed.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// An entry member and three peers, measured end to end: run with
/// `cargo test --release -p murmur-cli --test formation_launch -- --ignored --nocapture
/// four_member_formation_cost`.
#[test]
#[ignore]
fn four_member_formation_cost() {
    let _lock = launch_lock();
    let reps = 5;
    let mut runs: Vec<Value> = Vec::new();
    let shim_dir = TempDir::new().unwrap();
    let shim = build_dumpable_shim(shim_dir.path());
    for _ in 0..reps {
        let project = Project::new(
            &[CODER, REVIEWER, peer("tester", "0.1.0"), PLANNER],
            "reachability:\n  - from: planner\n    to: [coder, reviewer, tester]\n",
        );
        let cpu_before = children_cpu_seconds();
        let mut launcher = project.launch(&[], &[("LD_PRELOAD", shim.to_str().unwrap())]);
        let (peers_ready, formation) = launcher.next_json();
        let (entry_ready, planner) = launcher.next_json();
        project.await_entry_mid_task();
        let tree = process_tree(launcher.child.id());
        let (pss, smaps_unreadable) = pss_mb(&tree);
        let entry_pid = planner["pid"].as_u64().unwrap() as u32;
        project.release.send(()).unwrap();
        while alive(entry_pid) {
            thread::sleep(Duration::from_millis(5));
        }
        let entry_gone = Instant::now();
        let status = launcher.wait();
        let launcher_gone = Instant::now();
        assert_eq!(status.code(), Some(0), "{}", launcher.stderr());
        let cpu = children_cpu_seconds() - cpu_before;
        let containment: Vec<Value> = project
            .sessions()
            .iter()
            .filter_map(|session| std::fs::read_to_string(session.join("trace.jsonl")).ok())
            .filter_map(|trace| {
                trace
                    .lines()
                    .next()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok())
            })
            .map(|start| json!({start["capsule_name"].as_str().unwrap_or("?"): start["containment_achieved"]}))
            .collect();
        let ms = |from: Instant, to: Instant| (to - from).as_millis() as u64;
        let run = json!({
            "to_peers_ready_ms": ms(launcher.started, peers_ready),
            "to_entry_ready_ms": ms(launcher.started, entry_ready),
            "teardown_ms": ms(entry_gone, launcher_gone),
            "total_ms": ms(launcher.started, launcher_gone),
            "pss_mb": (pss * 10.0).round() / 10.0,
            "cpu_s": (cpu * 100.0).round() / 100.0,
            "processes": tree.len(),
            "smaps_unreadable": smaps_unreadable,
            "containment": containment,
        });
        println!("{run}");
        project.assert_no_member_remains(
            &reported_pids(&formation, Some(&planner)),
            Duration::from_secs(30),
        );
        runs.push(run);
    }
    let field = |name: &str| -> f64 {
        median(
            &mut runs
                .iter()
                .map(|run| run[name].as_f64().unwrap())
                .collect::<Vec<_>>(),
        )
    };
    println!(
        "{}",
        json!({
            "median": {
                "to_peers_ready_ms": field("to_peers_ready_ms"),
                "to_entry_ready_ms": field("to_entry_ready_ms"),
                "teardown_ms": field("teardown_ms"),
                "total_ms": field("total_ms"),
                "pss_mb": field("pss_mb"),
                "cpu_s": field("cpu_s"),
            }
        })
    );
}
