//! A delegating capsule killed outright takes its delegated children with it.
//!
//! Every case runs the real thing: `mur-roost` on a loopback port over a real registry, the
//! delegating capsule as its own `mur run` process — alone, or as a formation's entry member under
//! `mur run --roster` — and its children launched by its own runtime. The delegating process is
//! then ended by `SIGKILL`, or by its own clean exit, and what is measured is how long its
//! children take to notice through their spawner lifelines and go.
//!
//! Every child's directory is unique and is on its command line as `--workdir`, so "the child is
//! gone" is "no process on the host names that directory".

#![cfg(target_os = "linux")]

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::door_capsule::{message, rpc, DRIVER_NAME, DRIVER_VERSION};
use common::formation::{launch_lock, processes_mentioning};
use common::{
    event_kinds as kinds, never_replying, publish_to_store, read_whole_trace as read_trace, signal,
    tool_use_response, ScriptedServer,
};
use mur_roost::{authority::SpawnAuthority, State};
use serde_json::{json, Value};
use tempfile::TempDir;

const VERSION: &str = "0.1.0";
/// The sub-capsule whose model never answers: its task never leaves `working`, so nothing but its
/// spawner lifeline ends it.
const MUTE: &str = "mute-worker";
/// The sub-capsule that delegates to [`MUTE`] in its turn.
const RELAY: &str = "relay";

/// How long a child may take to go once its spawner has: the runtime's teardown deadline, with
/// room for a loaded host.
const ONE_LEVEL: Duration = Duration::from_secs(30);
/// The same for a grandchild, two levels below the process that was killed.
const TWO_LEVELS: Duration = Duration::from_secs(60);
/// How long a delegation may take to start.
const START_LIMIT: Duration = Duration::from_secs(240);

const SPAWNER_CLOSED: &str = "spawner lifeline closed — the session that delegated to this one \
                              has ended";

// ── A daemon, a registry and a HOME ───────────────────────────────────────────

/// One case's world: a scratch `HOME` whose store is the daemon's registry, the daemon, and the
/// mute worker's endpoint.
struct World {
    home: TempDir,
    registry: PathBuf,
    roost_url: String,
    _state: Arc<State>,
}

/// An agent capsule's manifest body against `endpoint`, reaching every loopback port, with
/// `extra` spliced into `capabilities` and `tail` appended.
fn agent_body(endpoint: &str, capabilities_extra: &str, lifecycle: &str, tail: &str) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
         capabilities:\n  network:\n    allow: [localhost, 127.0.0.1]\n{capabilities_extra}\
         {lifecycle}\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
         {tail}"
    )
}

const QUEUE_SLEEP: &str = "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n";

fn spawn_allow(names: &[&str]) -> String {
    format!("  spawn:\n    allow: [{}]\n", names.join(", "))
}

impl World {
    /// A daemon that admits `top_level` as registrants, over a registry holding the driver and the
    /// mute worker.
    fn new(top_level: &[&str]) -> Self {
        let home = TempDir::new().unwrap();
        let registry = home.path().join(".murmur").join("artifacts");
        std::fs::create_dir_all(&registry).unwrap();
        publish_to_store(
            &registry,
            DRIVER_NAME,
            DRIVER_VERSION,
            "driver",
            "runtime: driver\nupstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n",
            Some((
                "tool.wasm",
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            )),
        );
        publish_to_store(
            &registry,
            MUTE,
            VERSION,
            "capsule",
            &agent_body(&never_replying(), "", QUEUE_SLEEP, ""),
            None,
        );

        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            registry_path: registry.clone(),
            spawn_allow: top_level.iter().map(|name| name.to_string()).collect(),
            max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
            max_concurrent: mur_roost::bounds::DEFAULT_MAX_CONCURRENT,
            max_live_capsules: u32::MAX,
            inherited: Default::default(),
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let roost_url = format!("http://{}", listener.local_addr().unwrap());
        let accept = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&accept);
                thread::spawn(move || mur_roost::handle_connection(stream, state));
            }
        });
        Self {
            home,
            registry,
            roost_url,
            _state: state,
        }
    }

    fn publish(&self, name: &str, body: &str) {
        publish_to_store(&self.registry, name, VERSION, "capsule", body, None);
    }

    /// `mur <args>` in `dir`, under this world's `HOME` and daemon, with nothing a launch could
    /// mistake for a formation, a spawner or a lifeline inherited from this process.
    fn mur(&self, dir: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home.path())
            .env("MURMUR_ROOST_URL", &self.roost_url)
            .env_remove(capsule_runtime::MUR_BINARY_ENV)
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .env_remove(capsule_runtime::FORMATION_LIFELINE_ENV)
            .env_remove(capsule_runtime::SPAWNER_LIFELINE_ENV)
            .env_remove(capsule_runtime::delegation::SPAWNER_ENV);
        command
    }
}

/// A model whose first reply calls `delegate-task` on `capsule`, and whose every later request
/// is answered by `then` — or, with `None`, held unanswered for as long as the test runs.
fn delegating_model(capsule: &str, then: Option<String>) -> ScriptedServer {
    let calls = AtomicUsize::new(0);
    let capsule = capsule.to_string();
    ScriptedServer::start_answering(8, move |_| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return tool_use_response(
                "toolu_delegate",
                "delegate-task",
                json!({"capsule": capsule, "version": VERSION, "task": "hold the line"}),
            );
        }
        match &then {
            Some(reply) => reply.clone(),
            None => {
                thread::sleep(Duration::from_secs(900));
                common::door_capsule::end_turn(9, "too late")
            }
        }
    })
}

// ── A process this test started ───────────────────────────────────────────────

/// A `mur` process, its stdout lines as they arrive and its stderr kept.
struct Proc {
    child: Child,
    lines: mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl Proc {
    fn spawn(mut command: Command, tag: &'static str) -> Self {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
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
                eprintln!("[{tag}] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            }
        });
        Self {
            child,
            lines,
            stderr,
        }
    }

    /// The next stdout line that is a JSON object.
    fn next_json(&self) -> Value {
        let deadline = Instant::now() + START_LIMIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|_| panic!("no JSON line; stderr:\n{}", self.stderr()));
            if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&line) {
                return value;
            }
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// `SIGKILL`, and reap.
    fn kill(&mut self) {
        signal(self.pid(), libc::SIGKILL);
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A standalone delegating capsule: `mur run --manifest <project>/murmur.yaml --json`.
struct Parent {
    project: TempDir,
    proc: Proc,
    startup: Value,
    _model: ScriptedServer,
}

impl Parent {
    /// Publish `name` with `body` (its daemon envelope), write the same manifest into a project of
    /// its own, start it, and wait for its readiness line. `prepare` adjusts the command first.
    fn start(
        world: &World,
        name: &str,
        body: &str,
        model: ScriptedServer,
        prepare: impl FnOnce(&mut Command),
    ) -> Self {
        world.publish(name, body);
        let project = tempfile::Builder::new()
            .prefix("spawner-parent-")
            .tempdir()
            .unwrap();
        std::fs::write(
            project.path().join("murmur.yaml"),
            format!("name: {name}\nversion: {VERSION}\n{body}"),
        )
        .unwrap();
        let mut command = world.mur(
            project.path(),
            &["run", "--manifest", "murmur.yaml", "--json"],
        );
        prepare(&mut command);
        let proc = Proc::spawn(command, "parent");
        let startup = proc.next_json();
        Self {
            project,
            proc,
            startup,
            _model: model,
        }
    }

    fn url(&self) -> String {
        self.startup["url"].as_str().unwrap().to_string()
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn submit(&self) {
        let sent = rpc(
            &self.url(),
            None,
            "message/send",
            message("m-delegate", "delegate"),
        );
        assert_eq!(sent.status, 200, "{sent:?}");
    }
}

// ── Finding what a delegation left on disk ────────────────────────────────────

/// The first directory named `name` beneath `root`.
fn find_dir(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|file| file == name) {
                    return Some(path);
                }
                stack.push(path);
            }
        }
    }
    None
}

/// One delegation as the delegating session recorded it, and where its child lives.
struct Delegation {
    delegation_id: String,
    child_session: PathBuf,
}

impl Delegation {
    /// The child's own directory: `<child>/.murmur/<ses>`'s grandparent.
    fn child_dir(&self) -> PathBuf {
        self.child_session
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf()
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.child_session.join("trace.jsonl"))
    }

    fn bootstrap_log(&self) -> String {
        std::fs::read_to_string(self.child_session.join("logs").join("bootstrap.log"))
            .unwrap_or_default()
    }

    /// The child's own `mur run`: the one process whose `--workdir` is its directory.
    fn processes(&self) -> Vec<(u32, String)> {
        processes_mentioning(&format!("{} --json", self.child_dir().display()))
    }
}

/// Wait for the session `session_id`, somewhere under `root`, to record a `delegation_start`
/// whose child has written its own `session_start`.
fn await_delegation(root: &Path, session_id: &str) -> Delegation {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        if let Some(session) = find_dir(root, session_id) {
            let trace = std::fs::read_to_string(session.join("trace.jsonl")).unwrap_or_default();
            let events: Vec<Value> = trace
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            assert!(
                !events
                    .iter()
                    .any(|event| event["event_type"] == "delegation"
                        && (event["outcome"] == "failed" || event["outcome"] == "refused")),
                "the delegation never started: {trace}"
            );
            if let Some(start) = events
                .iter()
                .find(|event| event["event_type"] == "delegation_start")
            {
                let child = start["child_session_id"].as_str().unwrap();
                if let Some(child_session) = find_dir(root, child) {
                    if child_session.join("trace.jsonl").exists() {
                        return Delegation {
                            delegation_id: start["delegation_id"].as_str().unwrap().to_string(),
                            child_session,
                        };
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "{session_id} never started a delegation"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// How long until no process names `delegation`'s directory, failing past `limit`.
fn gone_within(delegation: &Delegation, since: Instant, limit: Duration) -> Duration {
    loop {
        if delegation.processes().is_empty() {
            return since.elapsed();
        }
        assert!(
            since.elapsed() < limit,
            "{} was still running {limit:?} later: {:?}",
            delegation.child_dir().display(),
            delegation.processes()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A child wound down because its spawner ended: one `spawner_ended` naming `spawned_by` and
/// `delegation_id`, ahead of its cancelled task, its task's end and `session_end`, which is last;
/// no `formation_ended`; and the reason in its `logs/bootstrap.log`, its stderr having gone with
/// its spawner.
fn assert_wound_down_by_spawner(delegation: &Delegation, spawned_by: &str) {
    let events = delegation.trace();
    let kinds = kinds(&events);
    let ended: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "spawner_ended")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(ended.len(), 1, "{kinds:?}");
    let record = &events[ended[0]];
    assert_eq!(record["spawned_by"], spawned_by, "{record}");
    assert_eq!(
        record["delegation_id"],
        delegation.delegation_id.as_str(),
        "{record}"
    );
    let canceled = kinds.iter().position(|kind| *kind == "task_canceled");
    let task_end = events
        .iter()
        .position(|event| event["event_type"] == "task_end" && event["exit_status"] == "canceled");
    let session_end = kinds.iter().position(|kind| *kind == "session_end");
    match (canceled, task_end, session_end) {
        (Some(canceled), Some(task_end), Some(session_end)) => assert!(
            ended[0] < canceled && canceled < task_end && task_end < session_end,
            "{kinds:?}"
        ),
        _ => panic!("the wind-down is incomplete: {kinds:?}"),
    }
    assert_eq!(kinds.last(), Some(&"session_end"), "{kinds:?}");
    assert!(!kinds.contains(&"formation_ended"), "{kinds:?}");
    let log = delegation.bootstrap_log();
    assert!(log.contains(SPAWNER_CLOSED), "{log}");
}

// ── S2: a parent in no formation ──────────────────────────────────────────────

/// `SIGKILL` of a delegating capsule in no formation, mid-task: its child — mid-task too — is gone
/// within one teardown deadline, its trace saying why.
#[test]
fn a_parent_killed_outright_takes_its_child_with_it() {
    if common::skip_without_host_support("a_parent_killed_outright_takes_its_child_with_it") {
        return;
    }
    let world = World::new(&["lone-parent"]);
    let model = delegating_model(MUTE, None);
    let body = agent_body(&model.endpoint, &spawn_allow(&[MUTE]), QUEUE_SLEEP, "");
    let mut parent = Parent::start(&world, "lone-parent", &body, model, |_| {});
    parent.submit();
    let delegation = await_delegation(parent.project.path(), &parent.session_id());
    assert!(
        !delegation.processes().is_empty(),
        "the child is not running"
    );

    let killed = Instant::now();
    parent.proc.kill();
    let took = gone_within(&delegation, killed, ONE_LEVEL);
    eprintln!(
        "[measure] S2 parent SIGKILL to child exit: {} ms",
        took.as_millis()
    );
    assert_wound_down_by_spawner(&delegation, &parent.session_id());
    let start = &delegation.trace()[0];
    assert_eq!(start["event_type"], "session_start");
    assert!(start.get("formation_id").is_none(), "{start}");
}

// ── S3: a child of a child ────────────────────────────────────────────────────

/// Parent → relay → mute worker, at the daemon's default depth bound. `SIGKILL` of the parent
/// ends the relay through its lifeline, and the relay's exit ends the grandchild through its own.
#[test]
fn a_grandchild_winds_down_once_its_parent_has() {
    if common::skip_without_host_support("a_grandchild_winds_down_once_its_parent_has") {
        return;
    }
    let world = World::new(&["root-parent"]);
    let relay_model = delegating_model(MUTE, None);
    world.publish(
        RELAY,
        &agent_body(
            &relay_model.endpoint,
            &spawn_allow(&[MUTE]),
            QUEUE_SLEEP,
            "",
        ),
    );
    let model = delegating_model(RELAY, None);
    let body = agent_body(
        &model.endpoint,
        &spawn_allow(&[RELAY, MUTE]),
        QUEUE_SLEEP,
        "",
    );
    let mut parent = Parent::start(&world, "root-parent", &body, model, |_| {});
    parent.submit();
    let relay = await_delegation(parent.project.path(), &parent.session_id());
    let relay_session = relay
        .child_session
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let grandchild = await_delegation(&relay.child_dir(), &relay_session);
    assert!(!relay.processes().is_empty() && !grandchild.processes().is_empty());

    let killed = Instant::now();
    parent.proc.kill();
    let relay_took = gone_within(&relay, killed, ONE_LEVEL);
    let grandchild_took = gone_within(&grandchild, killed, TWO_LEVELS);
    eprintln!(
        "[measure] S3 parent SIGKILL to relay exit: {} ms; to grandchild exit: {} ms",
        relay_took.as_millis(),
        grandchild_took.as_millis()
    );
    assert_wound_down_by_spawner(&relay, &parent.session_id());
    assert_wound_down_by_spawner(&grandchild, &relay_session);
}

// ── S4: a parent that exits on its own ────────────────────────────────────────

/// A capsule on the default lifecycle delegates, finishes its task and exits, as it always has.
/// `W-SEC-020` says its sub-capsules end with it, and they do.
#[test]
fn a_parent_that_exits_after_its_task_ends_its_child() {
    if common::skip_without_host_support("a_parent_that_exits_after_its_task_ends_its_child") {
        return;
    }
    let world = World::new(&["exiting-parent"]);
    let model = delegating_model(
        MUTE,
        Some(common::door_capsule::end_turn(2, "delegated; done")),
    );
    let body = agent_body(&model.endpoint, &spawn_allow(&[MUTE]), "", "");
    let mut parent = Parent::start(&world, "exiting-parent", &body, model, |_| {});
    parent.submit();

    let deadline = Instant::now() + START_LIMIT;
    let status = loop {
        if let Some(status) = parent.proc.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the parent never exited");
        thread::sleep(Duration::from_millis(50));
    };
    let exited = Instant::now();
    assert!(status.success(), "{status}: {}", parent.proc.stderr());
    let stderr = parent.proc.stderr();
    let warning = stderr
        .lines()
        .find(|line| line.contains("warning[W-SEC-020]"))
        .unwrap_or_else(|| panic!("no W-SEC-020: {stderr}"));
    assert!(
        warning.contains(
            "Every sub-capsule this capsule delegated to also winds down when this capsule \
             exits, finished or not."
        ),
        "{warning}"
    );

    let delegation = await_delegation(parent.project.path(), &parent.session_id());
    let took = gone_within(&delegation, exited, ONE_LEVEL);
    eprintln!(
        "[measure] S4 parent exit to child exit: {} ms",
        took.as_millis()
    );
    assert_wound_down_by_spawner(&delegation, &parent.session_id());
}

// ── S1: a formation member killed outright ────────────────────────────────────

/// `mur run --roster` whose entry member delegates to the mute worker; `SIGKILL` of that member
/// leaves the child to its spawner lifeline, and it goes, joined to the formation and recording
/// that its spawner — not its formation — ended.
#[test]
fn a_formation_member_killed_outright_takes_its_child_with_it() {
    if common::skip_without_host_support(
        "a_formation_member_killed_outright_takes_its_child_with_it",
    ) {
        return;
    }
    let _lock = launch_lock();
    let world = World::new(&["planner"]);
    let project = tempfile::Builder::new()
        .prefix("spawner-formation-")
        .tempdir()
        .unwrap();
    let store = project.path().join(".murmur").join("artifacts");
    let member_tail = "exports:\n  peer_tasks:\n    accept: true\n\
                       network:\n  authentication:\n    scheme: bearer\n";
    let planner_model = delegating_model(MUTE, None);
    // A capsule that may delegate declares no door authentication: its children post their
    // outcomes to that door.
    let planner = agent_body(
        &planner_model.endpoint,
        &spawn_allow(&[MUTE]),
        "lifecycle:\n  task_acceptance: single\n  after_task: exit\n",
        "",
    );
    let coder_model =
        ScriptedServer::start_answering(8, |_| common::door_capsule::end_turn(1, "done"));
    let coder = agent_body(
        &coder_model.endpoint,
        "",
        "lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n",
        member_tail,
    );
    // The daemon reads the entry member's envelope from its own registry; the launcher runs the
    // members from the project's store.
    world.publish("planner", &planner);
    publish_to_store(&store, "planner", VERSION, "capsule", &planner, None);
    publish_to_store(&store, "coder", VERSION, "capsule", &coder, None);
    std::fs::write(
        project.path().join("roster.yaml"),
        format!(
            "roster_version: 1\nmembers:\n  - name: coder\n    capsule: coder\n    version: \
             {VERSION}\n  - name: planner\n    capsule: planner\n    version: {VERSION}\n    \
             entry: true\n"
        ),
    )
    .unwrap();

    let launcher = Proc::spawn(
        world.mur(
            project.path(),
            &["run", "--roster", "--json", "--task", "delegate"],
        ),
        "launcher",
    );
    let formation = launcher.next_json();
    let entry = launcher.next_json();
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let entry_session = entry["session_id"].as_str().unwrap().to_string();
    let entry_pid = entry["pid"].as_u64().unwrap() as u32;
    let delegation = await_delegation(project.path(), &entry_session);
    assert!(
        !delegation.processes().is_empty(),
        "the child is not running"
    );

    let killed = Instant::now();
    signal(entry_pid, libc::SIGKILL);
    let took = gone_within(&delegation, killed, ONE_LEVEL);
    eprintln!(
        "[measure] S1 entry member SIGKILL to child exit: {} ms",
        took.as_millis()
    );
    assert_wound_down_by_spawner(&delegation, &entry_session);
    let start = &delegation.trace()[0];
    assert_eq!(start["event_type"], "session_start");
    assert_eq!(start["formation_id"], formation_id.as_str(), "{start}");
    drop(launcher);
    drop((planner_model, coder_model));
}

// ── S8: a kernel without CLOSE_RANGE_CLOEXEC ──────────────────────────────────

/// A seccomp filter that answers `close_range` with `ENOSYS` and allows everything else: the view
/// a process has of a kernel older than 5.11. Built once, before any fork, and never freed.
fn close_range_enosys_filter() -> &'static [libc::sock_filter] {
    // `BPF_LD | BPF_W | BPF_ABS`, `BPF_JMP | BPF_JEQ | BPF_K` and `BPF_RET | BPF_K`.
    const BPF_LD_W_ABS: u16 = 0x20;
    const BPF_JMP_JEQ_K: u16 = 0x15;
    const BPF_RET_K: u16 = 0x06;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    // `struct seccomp_data`: `nr` at 0, `arch` at 4.
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;
    let statement = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    Box::leak(Box::new([
        statement(BPF_LD_W_ABS, 0, 0, ARCH),
        statement(BPF_JMP_JEQ_K, 1, 0, AUDIT_ARCH),
        statement(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
        statement(BPF_LD_W_ABS, 0, 0, NR),
        statement(BPF_JMP_JEQ_K, 0, 1, libc::SYS_close_range as u32),
        statement(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
        statement(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ]))
}

/// Install `filter` in the process `command` starts, just before it execs. Filters are inherited
/// by every process it starts in turn.
#[allow(unsafe_code)]
fn under_filter(command: &mut Command, filter: &'static [libc::sock_filter]) {
    use std::os::unix::process::CommandExt;
    let (address, len) = (filter.as_ptr() as usize, filter.len() as u16);
    // SAFETY: the closure runs between `fork` and `exec` and makes two `prctl` calls, which are
    // async-signal-safe; the program it points at was built before the fork and is never freed.
    unsafe {
        command.pre_exec(move || {
            let program = libc::sock_fprog {
                len,
                filter: address as *mut libc::sock_filter,
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &program as *const libc::sock_fprog,
                ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// What `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)` returns in a process under `filter`, or under
/// none: `Ok` when it succeeded, the error otherwise. The call is made in the started process
/// itself, after the filter is installed, and its error is how the spawn fails.
#[allow(unsafe_code)]
fn close_range_in_a_process(filter: Option<&'static [libc::sock_filter]>) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("true");
    if let Some(filter) = filter {
        under_filter(&mut command, filter);
    }
    // SAFETY: one raw `close_range` syscall on integers, between `fork` and `exec`.
    unsafe {
        command.pre_exec(|| {
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            if libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.status().map(|_| ())
}

/// The S2 flow on a host that, as far as the parent and its child can tell, has no
/// `close_range`: the child is still launched — the parent-to-child path calls no `close_range` —
/// and still ends when its parent is killed.
#[test]
fn without_close_range_a_child_is_launched_and_still_ends_with_its_parent() {
    if common::skip_without_host_support(
        "without_close_range_a_child_is_launched_and_still_ends_with_its_parent",
    ) {
        return;
    }
    let filter = close_range_enosys_filter();
    close_range_in_a_process(None).expect("close_range works on this host unfiltered");
    let refused =
        close_range_in_a_process(Some(filter)).expect_err("close_range succeeded under the filter");
    assert_eq!(refused.raw_os_error(), Some(libc::ENOSYS), "{refused}");

    let world = World::new(&["filtered-parent"]);
    let model = delegating_model(MUTE, None);
    let body = agent_body(&model.endpoint, &spawn_allow(&[MUTE]), QUEUE_SLEEP, "");
    let mut parent = Parent::start(&world, "filtered-parent", &body, model, |command| {
        under_filter(command, filter)
    });
    parent.submit();
    let delegation = await_delegation(parent.project.path(), &parent.session_id());
    let (child_pid, _) = delegation.processes()[0].clone();
    let status = std::fs::read_to_string(format!("/proc/{child_pid}/status")).unwrap();
    assert!(
        status.lines().any(|line| line == "Seccomp:\t2"),
        "the child is not under a filter: {status}"
    );

    let killed = Instant::now();
    parent.proc.kill();
    let took = gone_within(&delegation, killed, ONE_LEVEL);
    eprintln!(
        "[measure] S8 (close_range -> ENOSYS) parent SIGKILL to child exit: {} ms",
        took.as_millis()
    );
    assert_wound_down_by_spawner(&delegation, &parent.session_id());
}
