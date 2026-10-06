//! Formation directories under `~/.murmur/formations/` are removed by the entry member's
//! `trace.retain`, at the end of a later `mur run --roster` from the same project, and never while
//! any part of the formation runs, never another project's, and never one with no marker.
//!
//! Every case runs a real two-member roster — entry `lead`, peer `worker` — under a scratch
//! `HOME`, against one scripted model on loopback that answers every request with an ended turn.
//! Fabricated formation directories are minted two days ago and carry a peer session, so the only
//! thing deciding whether one goes is the rule under test.

mod common;

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use capsule_runtime::formation_launch::{FormationMarker, FORMATION_MARKER_FILE};
use capsule_runtime::{FormationId, RunningRecord};
use common::door_capsule::{driver_home, end_turn, DRIVER_NAME, DRIVER_VERSION};
use common::formation::{
    alive, assert_no_member_remains, launch_lock, reported_pids, Launcher, LAUNCH_LIMIT,
};
use common::publish_to_store;
use common::recording_upstream::read_request;
use serde_json::Value;
use tempfile::TempDir;

const DAY_MS: u64 = 86_400_000;

// ── The model ────────────────────────────────────────────────────────────────

/// A model on loopback that answers every request with an ended turn, each on a thread of its
/// own, so one formation's held request never delays another's.
///
/// While a hold is armed, the next request to arrive takes it and is answered only once the hold
/// is released: that is how a case keeps one formation's entry member mid-task while later
/// launches from the same project run to their end.
struct Model {
    endpoint: String,
    arrived: Arc<AtomicUsize>,
    hold: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
}

impl Model {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let arrived = Arc::new(AtomicUsize::new(0));
        let hold: Arc<Mutex<Option<mpsc::Receiver<()>>>> = Arc::new(Mutex::new(None));
        let (counter, gate) = (Arc::clone(&arrived), Arc::clone(&hold));
        thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let (counter, gate) = (Arc::clone(&counter), Arc::clone(&gate));
                thread::spawn(move || answer(stream, &counter, &gate));
            }
        });
        Self {
            endpoint,
            arrived,
            hold,
        }
    }

    /// Hold the next request until the returned sender is sent to, or dropped.
    fn hold_next(&self) -> mpsc::Sender<()> {
        let (release, held) = mpsc::channel();
        *self.hold.lock().unwrap() = Some(held);
        release
    }

    /// Block until the armed hold has been taken by a request.
    fn await_held(&self) {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        while self.hold.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline, "no request took the hold");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn answer(mut stream: TcpStream, counter: &AtomicUsize, gate: &Mutex<Option<mpsc::Receiver<()>>>) {
    if read_request(&mut stream).is_none() {
        return;
    }
    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
    let held = gate.lock().unwrap().take();
    if let Some(held) = held {
        let _ = held.recv_timeout(LAUNCH_LIMIT);
    }
    let body = end_turn(n, "done");
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ── The project ──────────────────────────────────────────────────────────────

/// A member's manifest: the driver against `endpoint`, an authenticated door, `localhost` in
/// `capabilities.network.allow`, and `extra` appended.
fn member_manifest(endpoint: &str, lifecycle: &str, extra: &str) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
         capabilities:\n  network:\n    allow: [localhost]\n\
         lifecycle:\n  {lifecycle}\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
         exports:\n  peer_tasks:\n    accept: true\n\
         network:\n  authentication:\n    scheme: bearer\n{extra}"
    )
}

/// A project whose `roster.yaml` names `lead`, the entry member, and `worker`, both published
/// into its store, with a scratch `HOME` holding the driver.
struct Project {
    dir: TempDir,
    home: TempDir,
    model: Model,
}

impl Project {
    /// `lead_trace` is the `trace:` block of the entry member's manifest, or `""` for none.
    fn new(lead_trace: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("formation-prune-")
            .tempdir()
            .unwrap();
        let home = driver_home();
        let model = Model::start();
        let store = dir.path().join(".murmur").join("artifacts");
        let lead = member_manifest(
            &model.endpoint,
            "task_acceptance: single\n  after_task: exit",
            lead_trace,
        );
        let worker = member_manifest(
            &model.endpoint,
            "task_acceptance: queue\n  after_task: sleep",
            "",
        );
        publish_to_store(&store, "lead", "0.1.0", "capsule", &lead, None);
        publish_to_store(&store, "worker", "0.1.0", "capsule", &worker, None);
        std::fs::write(
            dir.path().join("roster.yaml"),
            "roster_version: 1\nmembers:\n  - name: lead\n    capsule: lead\n    version: 0.1.0\n    \
             entry: true\n  - name: worker\n    capsule: worker\n    version: 0.1.0\n\
             reachability:\n  - from: lead\n    to: [worker]\n",
        )
        .unwrap();
        Self { dir, home, model }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn formations(&self) -> PathBuf {
        self.home.path().join(".murmur").join("formations")
    }

    fn formation_dir(&self, id: &str) -> PathBuf {
        self.formations().join(id)
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
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .env_remove("MURMUR_SPAWNER");
        command
    }

    /// `mur run --roster --json --task probe`, started.
    fn launch(&self) -> Launcher {
        Launcher::spawn(self.command(&["run", "--roster", "--json", "--task", "probe"]))
    }

    /// One launch run to its end: its formation line, every pid it reported, and its stderr.
    fn run_formation(&self) -> Launched {
        let mut launcher = self.launch();
        let launched = Launched::started(&launcher);
        let status = launcher.wait();
        assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());
        launched.finished(&launcher)
    }

    /// An owned-looking formation directory minted two days ago, holding one peer session, with
    /// `marker` as its `formation.json` when given.
    fn fabricate(&self, tail: u64, marker: Option<&str>) -> String {
        let minted = now_ms() - 2 * DAY_MS;
        let id = format!("frm_{minted:012x}{tail:020x}");
        let dir = self.formation_dir(&id);
        let session = dir
            .join("worker")
            .join(".murmur")
            .join(format!("ses_{minted:012x}{tail:020x}"));
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("trace.jsonl"), "{}\n").unwrap();
        if let Some(marker) = marker {
            std::fs::write(dir.join(FORMATION_MARKER_FILE), marker).unwrap();
        }
        id
    }

    /// A marker naming this project, launched by `pid` with the start token `start`.
    fn marker(&self, pid: u32, start: &str) -> String {
        marker_naming(&std::fs::canonicalize(self.path()).unwrap(), pid, start)
    }

    fn assert_no_member_remains(&self, pids: &[u32]) {
        assert_no_member_remains(self.path(), pids, Duration::from_secs(30));
    }
}

fn marker_naming(project: &Path, pid: u32, start: &str) -> String {
    serde_json::to_string(&FormationMarker {
        project_dir: project.to_path_buf(),
        launcher_pid: pid,
        launcher_start: start.to_string(),
    })
    .unwrap()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A pid this test started and has already reaped: no process holds it.
fn reaped_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// A live process with its start token, killed and reaped when dropped.
struct Sleeper(Child);

impl Sleeper {
    fn start() -> Self {
        Self(Command::new("sleep").arg("120").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn token(&self) -> String {
        capsule_runtime::running::process_start_token(self.pid()).unwrap()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What one launch reported and wrote.
struct Launched {
    formation_id: String,
    pids: Vec<u32>,
    stdout: Vec<String>,
    stderr: String,
}

impl Launched {
    /// The formation line and the entry member's readiness line of a running launch.
    fn started(launcher: &Launcher) -> Self {
        let (_, formation) = launcher.next_json();
        let (_, entry) = launcher.next_json();
        Self {
            formation_id: formation["formation_id"].as_str().unwrap().to_string(),
            pids: reported_pids(&formation, Some(&entry)),
            stdout: Vec::new(),
            stderr: String::new(),
        }
    }

    /// This launch, with everything its launcher wrote, once it has exited.
    fn finished(mut self, launcher: &Launcher) -> Self {
        self.stdout = launcher.stdout();
        self.stderr = launcher.stderr();
        assert!(
            !self.stdout.iter().any(|line| line.contains("retention:")),
            "the retention pass wrote to stdout: {:?}",
            self.stdout
        );
        for line in &self.stdout {
            serde_json::from_str::<Value>(line)
                .unwrap_or_else(|_| panic!("stdout line is not JSON: {line}"));
        }
        self
    }

    /// Every `retention:` line on stderr.
    fn retention_lines(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains("retention:"))
            .collect()
    }
}

fn removed_line(id: &str, reason: &str) -> String {
    format!("[mur run] retention: removed formation {id} ({reason})")
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// `max_sessions: 2` keeps the two newest formation directories, the current one counted, and
/// `mur trace show` still reads a surviving formation's peers.
#[test]
fn max_sessions_keeps_the_newest_formations_and_trace_show_still_reads_them() {
    let _lock = launch_lock();
    let project = Project::new("trace:\n  retain:\n    max_sessions: 2\n");

    let first = project.run_formation();
    project.assert_no_member_remains(&first.pids);
    assert!(first.retention_lines().is_empty(), "{}", first.stderr);
    let second = project.run_formation();
    project.assert_no_member_remains(&second.pids);
    assert!(second.retention_lines().is_empty(), "{}", second.stderr);
    assert!(project.formation_dir(&first.formation_id).is_dir());
    assert!(project.formation_dir(&second.formation_id).is_dir());

    let third = project.run_formation();
    project.assert_no_member_remains(&third.pids);
    assert_eq!(
        third.retention_lines(),
        [removed_line(&first.formation_id, "max_sessions")],
        "{}",
        third.stderr
    );
    assert!(!project.formation_dir(&first.formation_id).exists());
    assert!(project.formation_dir(&second.formation_id).is_dir());
    assert!(project.formation_dir(&third.formation_id).is_dir());

    let workdir = project.path().join(".murmur");
    let shown = project
        .command(&["trace", "show", &second.formation_id, "--workdir"])
        .arg(&workdir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let worker_root = project
        .formation_dir(&second.formation_id)
        .join("worker")
        .join(".murmur");
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("searched:   {}", worker_root.display())),
        "{stdout}"
    );

    use std::os::unix::fs::PermissionsExt;
    let marker_path = project
        .formation_dir(&third.formation_id)
        .join(FORMATION_MARKER_FILE);
    let mode = std::fs::metadata(&marker_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let marker: Value =
        serde_json::from_str(&std::fs::read_to_string(&marker_path).unwrap()).unwrap();
    assert_eq!(
        marker["project_dir"],
        std::fs::canonicalize(project.path())
            .unwrap()
            .display()
            .to_string()
    );
}

/// `max_age: 1d` removes an ended two-day-old formation, and keeps one whose member still has a
/// live running record and one whose launcher is still alive.
#[test]
fn max_age_removes_an_ended_formation_and_keeps_a_live_one() {
    let _lock = launch_lock();
    let project = Project::new("trace:\n  retain:\n    max_age: 1d\n");
    let member = Sleeper::start();
    let launcher = Sleeper::start();

    let old_dead = project.fabricate(1, Some(&project.marker(reaped_pid(), "1")));
    let old_record = project.fabricate(2, Some(&project.marker(reaped_pid(), "1")));
    let old_launcher =
        project.fabricate(3, Some(&project.marker(launcher.pid(), &launcher.token())));

    let running = project.home.path().join(".murmur").join("running");
    std::fs::create_dir_all(&running).unwrap();
    let session_id = format!("ses_{:012x}{:020x}", now_ms() - 2 * DAY_MS, 2);
    let record = RunningRecord {
        session_id: session_id.clone(),
        url: "127.0.0.1:1".to_string(),
        pid: member.pid(),
        process_start: member.token(),
        capsule_name: "worker".to_string(),
        capsule_version: "0.1.0".to_string(),
        workdir: project.formation_dir(&old_record).join("worker"),
        outlives_launcher: false,
        started_at: "2026-01-01T00:00:00Z".to_string(),
        door_token: None,
        formation_id: Some(FormationId::parse(&old_record).unwrap()),
        formation_lifeline: false,
        formation_launcher: None,
        spawned_by: None,
    };
    let record_path = running.join(format!("{session_id}.json"));
    let record_json = serde_json::to_string(&record).unwrap();
    std::fs::write(&record_path, &record_json).unwrap();

    let launched = project.run_formation();
    project.assert_no_member_remains(&launched.pids);

    assert_eq!(
        launched.retention_lines(),
        [removed_line(&old_dead, "max_age")],
        "{}",
        launched.stderr
    );
    assert!(!project.formation_dir(&old_dead).exists());
    for id in [&old_record, &old_launcher, &launched.formation_id] {
        assert!(project.formation_dir(id).is_dir(), "{id}");
    }
    assert_eq!(std::fs::read_to_string(&record_path).unwrap(), record_json);
}

/// A formation whose entry member is mid-task outlives a later launch from the same project
/// under `max_sessions: 1`, and goes at the end of the launch after it has ended.
#[test]
fn a_live_formation_outlives_a_later_launch() {
    let _lock = launch_lock();
    let project = Project::new("trace:\n  retain:\n    max_sessions: 1\n");

    let release = project.model.hold_next();
    let mut held = project.launch();
    let a = Launched::started(&held);
    project.model.await_held();

    let b = project.run_formation();
    assert!(b.retention_lines().is_empty(), "{}", b.stderr);
    assert!(project.formation_dir(&a.formation_id).is_dir());
    // A's members still carry the project path, so B's are checked by pid alone until A ends.
    let deadline = Instant::now() + Duration::from_secs(30);
    while b.pids.iter().any(|pid| alive(*pid)) {
        assert!(
            Instant::now() < deadline,
            "B's members remain: {:?}",
            b.pids
        );
        thread::sleep(Duration::from_millis(100));
    }

    release.send(()).unwrap();
    let status = held.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", held.stderr());
    let a = a.finished(&held);
    project.assert_no_member_remains(&[a.pids.clone(), b.pids.clone()].concat());
    assert!(a.retention_lines().is_empty(), "{}", a.stderr);
    assert!(project.formation_dir(&b.formation_id).is_dir());

    let c = project.run_formation();
    project.assert_no_member_remains(&c.pids);
    assert_eq!(
        c.retention_lines(),
        [
            removed_line(&b.formation_id, "max_sessions"),
            removed_line(&a.formation_id, "max_sessions"),
        ],
        "{}",
        c.stderr
    );
    assert!(!project.formation_dir(&a.formation_id).exists());
    assert!(!project.formation_dir(&b.formation_id).exists());
    assert!(project.formation_dir(&c.formation_id).is_dir());
    assert!(project.model.arrived.load(Ordering::SeqCst) >= 3);
}

/// With no `trace.retain` on the entry member, no formation directory is ever removed.
#[test]
fn no_policy_removes_nothing() {
    let _lock = launch_lock();
    let project = Project::new("");
    let old = project.fabricate(1, Some(&project.marker(reaped_pid(), "1")));

    let mut ids = vec![old];
    for _ in 0..3 {
        let launched = project.run_formation();
        project.assert_no_member_remains(&launched.pids);
        assert!(launched.retention_lines().is_empty(), "{}", launched.stderr);
        ids.push(launched.formation_id);
    }
    for id in &ids {
        assert!(project.formation_dir(id).is_dir(), "{id}");
    }
}

/// A formation whose marker names another project, one with no marker, and one whose marker is
/// not JSON are never removed, however old and however ended.
#[test]
fn another_projects_and_unmarked_formations_are_never_removed() {
    let _lock = launch_lock();
    let project = Project::new("trace:\n  retain:\n    max_age: 1d\n    max_sessions: 1\n");
    let other = tempfile::tempdir().unwrap();
    let foreign = project.fabricate(
        1,
        Some(&marker_naming(
            &std::fs::canonicalize(other.path()).unwrap(),
            reaped_pid(),
            "1",
        )),
    );
    let unmarked = project.fabricate(2, None);
    let garbled = project.fabricate(3, Some("not json {"));

    let launched = project.run_formation();
    project.assert_no_member_remains(&launched.pids);

    assert!(launched.retention_lines().is_empty(), "{}", launched.stderr);
    for id in [&foreign, &unmarked, &garbled, &launched.formation_id] {
        assert!(project.formation_dir(id).is_dir(), "{id}");
    }
}
