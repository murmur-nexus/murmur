//! `mur run --roster`: a declared formation launched end to end, every member a real `mur run`
//! process against a scripted model, and torn down when the entry member's task ends.
//!
//! Every member carries the test's unique project path on its command line through `--workdir`,
//! which is how "no member process remains" is checked: no process on the host has that path on
//! its `/proc/<pid>/cmdline`, and every pid the launcher reported is dead.
//!
//! Every member holds a lifeline whose write end only the launcher holds, so however the
//! launcher ends — its own `SIGKILL` included — every member reads EOF, writes `formation_ended`
//! and winds down.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::door_capsule::{driver_home, message, rpc, DRIVER_NAME, DRIVER_VERSION};
use common::formation::{alive, launch_lock, reported_pids, signal, Launcher, LAUNCH_LIMIT};
use common::{
    assert_wound_down_by_formation as assert_wound_down, event_kinds, publish_to_store,
    read_whole_trace as read_trace, tool_result_text, tool_use_response, ScriptedServer,
};
use murmur_artifact::LocalRegistry;
use serde_json::{json, Value};
use tempfile::TempDir;

const PROBE_TOOL: &str = "formation-probe";
const PROBE_VERSION: &str = "0.1.0";
const PROBE_CALL: &str = "toolu_formation_probe";

// ── Artifacts ─────────────────────────────────────────────────────────────────

/// An agent capsule's manifest body: its driver against `endpoint`, an authenticated door that
/// serves peers, `lifecycle`, `formation-probe` installed, and `localhost` — where every member's
/// door is — in `capabilities.network.allow`.
fn member_manifest(endpoint: &str, lifecycle: &str, driver_version: &str) -> String {
    format!(
        "artifacts:\n  - name: {DRIVER_NAME}\n    version: {driver_version}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
         \x20 - name: {PROBE_TOOL}\n    version: {PROBE_VERSION}\n    runtime: tool\n\
         capabilities:\n  network:\n    allow: [localhost]\n\
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

/// One member's scripted model, and how many requests it has received, counted on arrival.
struct MemberModel {
    server: ScriptedServer,
    arrived: Arc<AtomicUsize>,
}

/// The entry member's model: it calls `formation-probe` with `input` once and then ends its turn,
/// holding the second reply until `release` is sent — so a case can look at a running formation
/// before its task ends.
fn entry_model(input: Value) -> (MemberModel, mpsc::Sender<()>) {
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let arrived = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&arrived);
    let server = ScriptedServer::start_answering(2, move |_| {
        let calls = counter.fetch_add(1, Ordering::SeqCst) + 1;
        if calls == 1 {
            tool_use_response(PROBE_CALL, PROBE_TOOL, input.clone())
        } else {
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(120));
            end_turn_response("probed")
        }
    });
    (MemberModel { server, arrived }, release)
}

/// A peer's model: with `input`, it calls `formation-probe` with it on its first request; every
/// other request ends the turn.
fn peer_model(input: Option<Value>) -> MemberModel {
    let arrived = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&arrived);
    let server = ScriptedServer::start_answering(64, move |_| {
        let calls = counter.fetch_add(1, Ordering::SeqCst) + 1;
        match &input {
            Some(input) if calls == 1 => tool_use_response(PROBE_CALL, PROBE_TOOL, input.clone()),
            _ => end_turn_response("done"),
        }
    });
    MemberModel { server, arrived }
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
/// `HOME` holding the driver. Every member has a scripted model of its own.
struct Project {
    dir: TempDir,
    home: TempDir,
    models: Vec<(&'static str, MemberModel)>,
    release: mpsc::Sender<()>,
}

impl Project {
    /// The entry member probes with `{}` — the names it was handed — and no peer probes.
    fn new(members: &[Member], reachability: &str) -> Self {
        Self::with_probes(members, reachability, &[])
    }

    /// As [`Self::new`], with `probes` naming the input each listed member's model calls
    /// `formation-probe` with.
    fn with_probes(members: &[Member], reachability: &str, probes: &[(&str, Value)]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("formation-launch-")
            .tempdir()
            .unwrap();
        let home = driver_home();
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
        let probe_of = |name: &str| {
            probes
                .iter()
                .find(|(member, _)| *member == name)
                .map(|(_, input)| input.clone())
        };
        let mut models = Vec::new();
        let mut release = None;
        let mut roster = String::from("roster_version: 1\nmembers:\n");
        for member in members {
            let driver_version = if member.broken {
                "9.9.9"
            } else {
                DRIVER_VERSION
            };
            let (model, lifecycle) = if member.entry {
                let (model, sender) = entry_model(probe_of(member.name).unwrap_or(json!({})));
                release = Some(sender);
                (model, "task_acceptance: single\n  after_task: exit")
            } else {
                (peer_model(probe_of(member.name)), PEER_LIFECYCLE)
            };
            let body = member_manifest(&model.server.endpoint, lifecycle, driver_version);
            publish_to_store(&store, member.name, member.version, "capsule", &body, None);
            models.push((member.name, model));
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
        // A roster with no entry member still needs a sender to drop.
        let release = release.unwrap_or_else(|| mpsc::channel().0);
        Self {
            dir,
            home,
            models,
            release,
        }
    }

    /// The scripted model of the member `name`.
    fn model(&self, name: &str) -> &MemberModel {
        &self
            .models
            .iter()
            .find(|(member, _)| *member == name)
            .unwrap_or_else(|| panic!("no member {name}"))
            .1
    }

    /// The entry member's model.
    fn entry_model(&self) -> &ScriptedServer {
        &self.model("planner").server
    }

    /// Block until `member`'s model has received `count` requests.
    fn await_requests(&self, member: &str, count: usize) {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        while self.model(member).arrived.load(Ordering::SeqCst) < count {
            assert!(
                Instant::now() < deadline,
                "{member}'s model never received {count} requests"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Block until the entry member is mid-task: its model holds the reply to the probe's result.
    fn await_entry_mid_task(&self) {
        self.await_requests("planner", 2);
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
        self.launch_with(extra, env, false)
    }

    /// [`Project::launch`], with the launcher leading a process group of its own when
    /// `own_group`, so the group can be killed without killing this test.
    fn launch_with(&self, extra: &[&str], env: &[(&str, &str)], own_group: bool) -> Launcher {
        let mut args = vec!["run", "--roster", "--json", "--task", "probe"];
        args.extend_from_slice(extra);
        let mut command = self.command(&args);
        command.envs(env.iter().copied());
        if own_group {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Launcher::spawn(command)
    }

    /// Every `ses_*` session directory members created: the entry member's under the project,
    /// each peer's under its own directory in the scratch `HOME`.
    fn sessions(&self) -> Vec<PathBuf> {
        std::iter::once(self.path().join(".murmur"))
            .chain(common::formation::peer_session_roots(self.home.path()))
            .flat_map(|root| common::formation::session_dirs(&root))
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

    /// The one session `capsule` ran in this project.
    fn session_of(&self, capsule: &str) -> PathBuf {
        let sessions = self.sessions_of(capsule);
        assert_eq!(sessions.len(), 1, "{capsule}: {sessions:?}");
        sessions.into_iter().next().unwrap()
    }

    /// Every record of `capsule`'s `trace.jsonl`, each line of which parses, ending in a newline.
    fn trace_of(&self, capsule: &str) -> Vec<Value> {
        read_trace(&self.session_of(capsule).join("trace.jsonl"))
    }

    /// `capsule`'s `logs/bootstrap.log`, where a diagnostic lands once its stderr is a broken pipe.
    fn bootstrap_log_of(&self, capsule: &str) -> String {
        std::fs::read_to_string(self.session_of(capsule).join("logs").join("bootstrap.log"))
            .unwrap_or_default()
    }

    /// The operator token the running record of `session_id` holds.
    fn door_token(&self, session_id: &str) -> String {
        common::formation::door_token(self.home.path(), session_id)
    }

    /// Fail unless no process on the host carries this project's path on its command line within
    /// `limit`.
    fn assert_no_member_remains(&self, pids: &[u32], limit: Duration) {
        common::formation::assert_no_member_remains(self.path(), pids, limit);
    }
}

struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
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
        assert_eq!(
            keys,
            ["name", "pid", "session_id", "url", "workdir"],
            "{peer}"
        );
        assert!(peer["session_id"].as_str().unwrap().starts_with("ses_"));
        assert!(peer["url"].as_str().unwrap().starts_with("http://"));
        // The peer's own directory, `<home>/.murmur/formations/<frm_id>/<member>`, owner-only.
        let workdir = Path::new(peer["workdir"].as_str().unwrap());
        assert!(
            workdir.ends_with(
                Path::new("formations")
                    .join(id)
                    .join(peer["name"].as_str().unwrap())
            ),
            "{peer}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(workdir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{peer}");
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

/// What a member logs when its lifeline reads EOF.
const LIFELINE_CLOSED: &str = "[capsule-runtime] formation lifeline closed";

/// The entry member's in-flight task ended cancelled.
fn assert_task_canceled(events: &[Value]) {
    let kinds = event_kinds(events);
    assert!(kinds.contains(&"task_canceled"), "{kinds:?}");
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "task_end" && event["exit_status"] == "canceled"),
        "{kinds:?}"
    );
}

#[allow(unsafe_code)]
fn signal_group(pgid: u32, signal: i32) {
    // SAFETY: `kill` takes two integers and dereferences nothing; `pgid` is the group of a child
    // this test started in a group of its own.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), signal);
    }
}

// ── Scenarios ─────────────────────────────────────────────────────────────────

/// A chain: `planner` may call `coder`, and `coder` may call `reviewer`.
const CHAIN_REACH: &str =
    "reachability:\n  - from: planner\n    to: [coder]\n  - from: coder\n    to: [reviewer]\n";

/// The chain's probes: `planner` tries a callee, a member it may not call and a name that is no member;
/// `coder`, on the task `planner` sends it, tries its callee and the entry member.
fn chain_probes() -> [(&'static str, Value); 2] {
    [
        ("planner", json!({"names": ["coder", "reviewer", "nosuch"]})),
        ("coder", json!({"names": ["reviewer", "planner"]})),
    ]
}

/// The probe's line for `name`, with the name taken off: what a refusal says, compared across
/// names.
fn outcome_of<'a>(lines: &'a [&'a str], name: &str) -> &'a str {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap_or_else(|| panic!("no line for {name}: {lines:?}"))
}

/// The `session_start` and every `a2a_task_received` of the one session `capsule` ran.
fn member_trace(project: &Project, capsule: &str) -> (Value, Vec<Value>) {
    let sessions = project.sessions_of(capsule);
    assert_eq!(sessions.len(), 1, "{capsule}: {sessions:?}");
    let trace = std::fs::read_to_string(sessions[0].join("trace.jsonl")).unwrap();
    let events: Vec<Value> = trace
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let received = events
        .iter()
        .filter(|event| event["event_type"] == "a2a_task_received")
        .cloned()
        .collect();
    (events[0].clone(), received)
}

/// Every file under `root` holding `needle`.
fn files_holding(root: &Path, needle: &str) -> Vec<PathBuf> {
    common::door_capsule::files_containing(root, needle)
}

/// The roster's reachability is enforced end to end. `planner` reaches
/// `coder` and cannot address `reviewer`; `coder` reaches `reviewer` and cannot address the entry
/// member; every door answers an unauthenticated or forged call `401`; no token appears in
/// anything the run produced; and once the launcher exits, every door is gone.
#[test]
fn a_formation_reaches_exactly_what_its_roster_lets_it() {
    let _lock = launch_lock();
    let project = Project::with_probes(&[CODER, REVIEWER, PLANNER], CHAIN_REACH, &chain_probes());
    let mut launcher = project.launch(&[], &[]);

    let (_, formation) = launcher.next_json();
    assert_formation_line(&formation, "planner", &["coder", "reviewer"]);
    let (_, planner) = launcher.next_json();
    assert_eq!(
        planner["formation_id"], formation["formation_id"],
        "{planner}"
    );
    assert_eq!(planner["name"], "planner");

    // While up, every door refuses a caller with no credential, and one presenting a formation
    // token that does not verify, with the ordinary bodies.
    let mut doors: Vec<String> = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| {
            peer["url"]
                .as_str()
                .unwrap()
                .trim_start_matches("http://")
                .to_string()
        })
        .collect();
    doors.push(planner["url"].as_str().unwrap().to_string());
    for (token, code) in [
        (None, "unauthenticated"),
        (Some("mft1.x.y"), "invalid_token"),
    ] {
        let mut bodies = Vec::new();
        for door in &doors {
            let refused = rpc(door, token, "message/send", message("msg_s2", "hello"));
            assert_eq!(refused.status, 401, "{door} {token:?}: {}", refused.body);
            assert_eq!(refused.json()["error"], code, "{door}: {}", refused.body);
            bodies.push(refused.body);
        }
        // Only the challenge's realm names the member; the body is the same at every door.
        assert!(
            bodies.windows(2).all(|pair| pair[0] == pair[1]),
            "{bodies:?}"
        );
    }

    // Each peer's door answers as the session the formation line names, to its operator token.
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

    // `coder` has run `planner`'s task and probed; `reviewer` has taken `coder`'s.
    project.await_requests("coder", 2);
    project.await_requests("reviewer", 1);
    // While every member runs, its running record and session directory hold no formation token.
    for root in [project.path(), project.home.path()] {
        let holding = files_holding(root, "mft1.");
        assert!(holding.is_empty(), "while up: {holding:?}");
    }
    project.release.send(()).unwrap();
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let planner_result = tool_result_text(&project.entry_model().requests(), PROBE_CALL)
        .expect("planner's probe result reached its model");
    println!("planner's probe:\n{planner_result}");
    let lines: Vec<&str> = planner_result.lines().collect();
    assert_eq!(lines.len(), 4, "{planner_result}");
    assert_eq!(outcome_of(&lines, "coder"), "card=200 send=200");
    let reviewer = outcome_of(&lines, "reviewer");
    assert!(reviewer.starts_with("card=refused:"), "{reviewer}");
    assert_eq!(reviewer, outcome_of(&lines, "nosuch"), "{planner_result}");
    assert_eq!(lines[3], "peers=coder=http://coder.formation.invalid");

    let coder_result = tool_result_text(&project.model("coder").server.requests(), PROBE_CALL)
        .expect("coder's probe result reached its model");
    println!("coder's probe:\n{coder_result}");
    let lines: Vec<&str> = coder_result.lines().collect();
    assert_eq!(lines.len(), 3, "{coder_result}");
    assert_eq!(outcome_of(&lines, "reviewer"), "card=200 send=200");
    assert_eq!(
        outcome_of(&lines, "planner"),
        reviewer,
        "the entry member is refused as an unknown name is"
    );
    assert_eq!(lines[2], "peers=reviewer=http://reviewer.formation.invalid");

    // The trace names each member, its callees, and who called it.
    for (capsule, callees, callers) in [
        ("planner", json!(["coder"]), Vec::<&str>::new()),
        ("coder", json!(["reviewer"]), vec!["planner"]),
        ("reviewer", json!([]), vec!["coder"]),
    ] {
        let (start, received) = member_trace(&project, capsule);
        assert_eq!(start["formation_member"], capsule, "{start}");
        assert_eq!(start["formation_callees"], callees, "{start}");
        let seen: Vec<&str> = received
            .iter()
            .map(|event| event["caller_member"].as_str().unwrap_or("<none>"))
            .collect();
        assert_eq!(seen, callers, "{capsule}");
    }

    let stderr = launcher.stderr();
    assert!(!stderr.contains("W-RUN-005"), "{stderr}");
    assert!(!stderr.contains("W-RUN-006"), "{stderr}");

    // No formation token anywhere the run wrote, and no door token but the entry member's own
    // readiness-line tokens.
    let stdout = launcher.stdout();
    assert_eq!(stdout.len(), 2, "{stdout:?}");
    assert_no_formation_token(&project, &stdout, &stderr);
    assert_no_token_but_the_entrys(&stdout, &stderr);

    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );

    // The entry member ended on its own; its exit is what closed the peers' lifelines.
    let id = formation["formation_id"].as_str().unwrap();
    for peer in ["coder", "reviewer"] {
        assert_wound_down(&project.trace_of(peer), id);
    }
    let planner_kinds = event_kinds(&project.trace_of("planner")).join(" ");
    assert!(
        !planner_kinds.contains("formation_ended"),
        "{planner_kinds}"
    );
    assert!(
        !stderr.contains("SIGTERM received"),
        "the launcher signalled a member: {stderr}"
    );

    // Once the launcher has exited, no reported door answers.
    for door in &doors {
        assert!(
            std::net::TcpStream::connect(door).is_err(),
            "{door} still accepts connections"
        );
    }
    // A second launch of the same roster is a new formation.
    let again = Project::with_probes(
        &[CODER, REVIEWER, PLANNER],
        CHAIN_REACH,
        &[("planner", json!({"names": []}))],
    );
    let mut relaunched = again.launch(&[], &[]);
    let (_, second) = relaunched.next_json();
    assert_ne!(second["formation_id"], formation["formation_id"]);
    let (_, second_planner) = relaunched.next_json();
    again.release.send(()).unwrap();
    assert_eq!(relaunched.wait().code(), Some(0), "{}", relaunched.stderr());
    again.assert_no_member_remains(
        &reported_pids(&second, Some(&second_planner)),
        Duration::from_secs(30),
    );
}

/// `mft1.` appears nowhere the run produced — the launcher's two streams, any file under the
/// project or the scratch `HOME` (every session directory and running record among them), or any
/// request any member's model was sent.
fn assert_no_formation_token(project: &Project, stdout: &[String], stderr: &str) {
    assert!(
        !stdout.iter().any(|line| line.contains("mft1.")),
        "{stdout:?}"
    );
    assert!(
        !stderr.contains("mft1."),
        "stderr carries a formation token"
    );
    for root in [project.path(), project.home.path()] {
        let holding = files_holding(root, "mft1.");
        assert!(
            holding.is_empty(),
            "files hold a formation token: {holding:?}"
        );
    }
    for (member, model) in &project.models {
        let requests = serde_json::to_string(&model.server.requests()).unwrap();
        assert!(
            !requests.contains("mft1."),
            "{member}'s model was sent a formation token"
        );
    }
}

/// The entry member's task is written to the project directory and left behind there. A peer
/// runs in a directory of its own and takes work only at its door: a `task.md` already in the
/// project — an earlier launch's — is never run by a peer.
#[test]
fn a_task_file_in_the_project_is_never_a_peers_task() {
    let _lock = launch_lock();
    let project = Project::with_probes(
        &[CODER, REVIEWER, PLANNER],
        FULL_REACH,
        &[("planner", json!({"names": []}))],
    );
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
            vec![
                "coder card=200 send=200",
                "peers=coder=http://coder.formation.invalid",
            ],
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

        let result = tool_result_text(&project.entry_model().requests(), PROBE_CALL).unwrap();
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
            project.entry_model().requests().is_empty(),
            "planner never asked its model"
        );
        project.assert_no_member_remains(&pids, Duration::from_secs(30));
        assert!(!stderr.contains("mdt1."), "{stderr}");

        // Every peer that got as far as a session heard its lifeline close, whether or not it had
        // become ready by the time the launch was refused. Peers start alongside `broken`, so the
        // refusal can land before either has staged one.
        let started: Vec<PathBuf> = ["coder", "reviewer"]
            .iter()
            .flat_map(|peer| project.sessions_of(peer))
            .collect();
        for session in started {
            let events = read_trace(&session.join("trace.jsonl"));
            let id = events[0]["formation_id"].as_str().unwrap().to_string();
            assert_wound_down(&events, &id);
        }
        assert!(!stderr.contains("SIGTERM received"), "{stderr}");
    }
}

/// Scenario 5: a signal to the launcher, and the entry member's own death, both end the whole
/// formation with the documented status, and no member's trace is torn.
#[test]
fn the_entry_members_end_or_the_launchers_signal_ends_the_formation() {
    let _lock = launch_lock();
    for (kill_launcher, expected) in [(true, 143), (false, 137)] {
        let project = Project::with_probes(
            &[CODER, REVIEWER, PLANNER],
            FULL_REACH,
            &[("planner", json!({"names": []}))],
        );
        let mut launcher = project.launch(&[], &[]);
        let (_, formation) = launcher.next_json();
        let (_, planner) = launcher.next_json();
        project.await_entry_mid_task();
        if kill_launcher {
            launcher.signal(libc::SIGTERM);
        } else {
            signal(planner["pid"].as_u64().unwrap() as u32, libc::SIGKILL);
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

        let id = formation["formation_id"].as_str().unwrap();
        let stderr = launcher.stderr();
        for peer in ["coder", "reviewer"] {
            assert_wound_down(&project.trace_of(peer), id);
        }
        if kill_launcher {
            // The entry member's lifeline is closed rather than a `SIGTERM` forwarded to it.
            let planner_trace = project.trace_of("planner");
            assert_wound_down(&planner_trace, id);
            assert_task_canceled(&planner_trace);
        } else {
            for peer in ["coder", "reviewer"] {
                assert!(
                    stderr.contains(&format!("[{peer}] {LIFELINE_CLOSED}")),
                    "{stderr}"
                );
            }
        }
        assert!(!stderr.contains("SIGTERM received"), "{stderr}");
    }
}

/// The kill path: `SIGKILL` of the launcher leaves nothing to stop the members, and every member
/// — the entry member mid-task included — reads EOF on its lifeline and winds down in order.
#[test]
fn a_killed_launcher_winds_down_every_member() {
    let _lock = launch_lock();
    let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
    let mut launcher = project.launch(&[], &[]);
    let (_, formation) = launcher.next_json();
    let (_, planner) = launcher.next_json();
    project.await_entry_mid_task();

    let killed = Instant::now();
    launcher.signal(libc::SIGKILL);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(launcher.wait().signal(), Some(libc::SIGKILL));
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
    eprintln!(
        "[measure] launcher SIGKILL to the last member's exit: {} ms",
        killed.elapsed().as_millis()
    );

    let id = formation["formation_id"].as_str().unwrap();
    for capsule in ["coder", "reviewer", "planner"] {
        assert_wound_down(&project.trace_of(capsule), id);
    }
    assert_task_canceled(&project.trace_of("planner"));

    // A peer's stderr was the dead launcher's pipe, so its diagnostics fell back to its
    // bootstrap log. The entry member inherited the launcher's stderr, which this test still reads.
    for peer in ["coder", "reviewer"] {
        let log = project.bootstrap_log_of(peer);
        assert!(log.contains(LIFELINE_CLOSED), "{peer}: {log}");
        assert!(!log.contains("SIGTERM received"), "{peer}: {log}");
    }
    let planner_said = format!(
        "{}{}",
        project.bootstrap_log_of("planner"),
        launcher.stderr()
    );
    assert!(planner_said.contains(LIFELINE_CLOSED), "{planner_said}");
    assert!(!planner_said.contains("SIGTERM received"), "{planner_said}");
}

/// `SIGKILL` of the launcher's whole process group takes the launcher and the entry member at once;
/// both peers wind down on their lifelines.
#[test]
fn a_killed_launcher_group_winds_down_both_peers() {
    let _lock = launch_lock();
    let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
    let mut launcher = project.launch_with(&[], &[], true);
    let (_, formation) = launcher.next_json();
    let (_, planner) = launcher.next_json();
    project.await_entry_mid_task();

    signal_group(launcher.child.id(), libc::SIGKILL);
    launcher.wait();
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
    let id = formation["formation_id"].as_str().unwrap();
    for peer in ["coder", "reviewer"] {
        assert_wound_down(&project.trace_of(peer), id);
        let log = project.bootstrap_log_of(peer);
        assert!(log.contains(LIFELINE_CLOSED), "{peer}: {log}");
    }
}

/// One descriptor on `/proc/<pid>/fd`: its number, what it points at, and its open flags.
#[cfg(target_os = "linux")]
struct OpenFd {
    fd: u32,
    target: String,
    flags: u32,
}

#[cfg(target_os = "linux")]
impl OpenFd {
    /// The inode of the pipe this descriptor is an end of.
    fn pipe_inode(&self) -> Option<u64> {
        self.target
            .strip_prefix("pipe:[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|inode| inode.parse().ok())
    }

    fn access_mode(&self) -> u32 {
        self.flags & libc::O_ACCMODE as u32
    }

    fn close_on_exec(&self) -> bool {
        self.flags & libc::O_CLOEXEC as u32 != 0
    }
}

/// Every descriptor `pid` holds, read from `/proc/<pid>/fd` and `/proc/<pid>/fdinfo`.
#[cfg(target_os = "linux")]
fn open_fds(pid: u32) -> Vec<OpenFd> {
    let mut fds = Vec::new();
    let dir = format!("/proc/{pid}/fd");
    for entry in std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("{dir}: {error}"))
        .flatten()
    {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Ok(info) = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else {
            continue;
        };
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
            .unwrap();
        fds.push(OpenFd {
            fd,
            target: target.to_string_lossy().into_owned(),
            flags,
        });
    }
    fds
}

/// The value of `name` in `pid`'s initial environment.
#[cfg(target_os = "linux")]
fn environ_value(pid: u32, name: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    raw.split(|byte| *byte == 0).find_map(|pair| {
        let pair = String::from_utf8_lossy(pair);
        pair.strip_prefix(&format!("{name}=")).map(str::to_string)
    })
}

/// While a formation is up, the launcher holds exactly one write end of each member's lifeline and
/// no read end; each member holds exactly its own read end, close-on-exec, at the number its
/// environment names; and no member or descendant holds any other end.
#[cfg(target_os = "linux")]
#[test]
fn lifeline_ends_are_held_once() {
    let _lock = launch_lock();
    // `mur` makes itself non-dumpable, which hands `/proc/<pid>/fd` and `environ` to root.
    let shim_dir = TempDir::new().unwrap();
    let shim = build_dumpable_shim(shim_dir.path());
    let project = Project::new(&[CODER, REVIEWER, PLANNER], FULL_REACH);
    let mut launcher = project.launch(&[], &[("LD_PRELOAD", shim.to_str().unwrap())]);
    let (_, formation) = launcher.next_json();
    let (_, planner) = launcher.next_json();
    project.await_entry_mid_task();

    let mut members: Vec<(String, u32)> = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| {
            (
                peer["name"].as_str().unwrap().to_string(),
                peer["pid"].as_u64().unwrap() as u32,
            )
        })
        .collect();
    members.push((
        "planner".to_string(),
        planner["pid"].as_u64().unwrap() as u32,
    ));

    // Each member's own read end.
    let mut lifelines: std::collections::HashMap<u64, (String, u32)> = Default::default();
    for (name, pid) in &members {
        let named: u32 = environ_value(*pid, capsule_runtime::FORMATION_LIFELINE_ENV)
            .unwrap_or_else(|| panic!("{name} was handed no lifeline"))
            .parse()
            .unwrap();
        let fds = open_fds(*pid);
        let own = fds
            .iter()
            .find(|fd| fd.fd == named)
            .unwrap_or_else(|| panic!("{name} does not hold descriptor {named}"));
        let inode = own
            .pipe_inode()
            .unwrap_or_else(|| panic!("{name}'s lifeline is {}", own.target));
        assert_eq!(own.access_mode(), libc::O_RDONLY as u32, "{name}");
        assert!(
            own.close_on_exec(),
            "{name}'s read end is inherited by what it spawns"
        );
        assert_eq!(
            fds.iter()
                .filter(|fd| fd.pipe_inode() == Some(inode))
                .count(),
            1,
            "{name} holds its lifeline twice"
        );
        assert!(
            lifelines.insert(inode, (name.clone(), named)).is_none(),
            "two members share a lifeline"
        );
    }

    // The launcher: one write end per lifeline, and no read end.
    let launcher_fds = open_fds(launcher.child.id());
    for (inode, (name, _)) in &lifelines {
        let held: Vec<&OpenFd> = launcher_fds
            .iter()
            .filter(|fd| fd.pipe_inode() == Some(*inode))
            .collect();
        assert_eq!(held.len(), 1, "the launcher's ends of {name}'s lifeline");
        assert_eq!(held[0].access_mode(), libc::O_WRONLY as u32, "{name}");
    }

    // No member, nor anything a member started, holds any other end of any lifeline.
    for (name, pid) in &members {
        for process in process_tree(*pid) {
            for fd in open_fds(process) {
                let Some((owner, number)) = fd.pipe_inode().and_then(|inode| lifelines.get(&inode))
                else {
                    continue;
                };
                assert!(
                    process == *pid && owner == name && fd.fd == *number,
                    "process {process} under {name} holds descriptor {} of {owner}'s lifeline",
                    fd.fd
                );
            }
        }
    }

    project.release.send(()).unwrap();
    assert_eq!(launcher.wait().code(), Some(0), "{}", launcher.stderr());
    project.assert_no_member_remains(
        &reported_pids(&formation, Some(&planner)),
        Duration::from_secs(30),
    );
}

/// `MURMUR_FORMATION_PEERS` in `mur run`'s own environment, with or without a formation id, is
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
                &project.entry_model().endpoint,
                PEER_LIFECYCLE,
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

/// An edge into the entry member is refused at admission, `E-ROS-008`, naming the rule's caller
/// and the entry member, with the turned-around edge as the fix. Nothing starts, nothing is
/// minted and nothing is written.
#[test]
fn an_edge_into_the_entry_member_is_refused_before_anything_starts() {
    let _lock = launch_lock();
    let project = Project::with_probes(
        &[CODER, REVIEWER, PLANNER],
        "reachability:\n  - from: planner\n    to: [coder]\n  - from: coder\n    to: [reviewer, planner]\n",
        &[
            ("planner", json!({"names": ["coder"]})),
            ("coder", json!({"names": ["reviewer", "planner", "nosuch"]})),
        ],
    );
    let mut launcher = project.launch(&[], &[]);
    let status = launcher.wait();
    let stderr = launcher.stderr();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("error[E-ROS-008]"), "{stderr}");
    assert!(
        stderr.contains("the reachability rule from 'coder' lists the entry member 'planner'"),
        "{stderr}"
    );
    let hint = &stderr[stderr.find("hint: ").expect("a hint")..];
    assert!(hint.contains("- from: planner"), "{stderr}");
    assert!(hint.contains("to: [coder]"), "{stderr}");
    assert!(!stderr.contains("W-RUN-006"), "{stderr}");
    let stdout = launcher.stdout();
    assert!(
        !stdout.iter().any(|line| line.contains("formation_id")),
        "{stdout:?}"
    );

    for member in ["planner", "coder", "reviewer"] {
        assert!(
            project.model(member).server.requests().is_empty(),
            "{member}'s model was called"
        );
    }
    assert!(project.sessions().is_empty(), "a refusal created a session");
    let formations = project.home.path().join(".murmur").join("formations");
    assert!(
        std::fs::read_dir(&formations).map_or(true, |mut entries| entries.next().is_none()),
        "a refusal created a formation directory under {}",
        formations.display()
    );
    project.assert_no_member_remains(&[], Duration::from_secs(1));
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

/// The member flags a launcher passes: `--ignore-task-file` is an unknown argument, and the hidden
/// `--store-root` is not shown, needs `--capsule` and `--workdir`, and conflicts with `--roster`.
#[test]
fn the_hidden_member_flags_are_exactly_what_a_launcher_passes() {
    let dir = tempfile::tempdir().unwrap();
    let mur = |args: &[&str]| {
        Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .args(args)
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .output()
            .unwrap()
    };
    let removed = mur(&[
        "run",
        "--ignore-task-file",
        "--capsule",
        "c",
        "--capsule-version",
        "1",
    ]);
    assert_eq!(removed.status.code(), Some(2), "{removed:?}");
    assert!(
        String::from_utf8_lossy(&removed.stderr)
            .contains("unexpected argument '--ignore-task-file'"),
        "{removed:?}"
    );

    let help = mur(&["run", "--help"]);
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--store-root"));

    let store = dir.path().display().to_string();
    for (args, refusal) in [
        (
            vec!["run", "--store-root", &store],
            "the following required arguments were not provided",
        ),
        (
            vec!["run", "--store-root", &store, "--workdir", &store],
            "the following required arguments were not provided",
        ),
        (
            vec![
                "run",
                "--store-root",
                &store,
                "--capsule",
                "c",
                "--capsule-version",
                "1",
            ],
            "the following required arguments were not provided",
        ),
        // Every argument `--store-root` requires is given, so only the conflict can refuse it.
        (
            vec![
                "run",
                "--roster",
                &store,
                "--store-root",
                &store,
                "--capsule",
                "c",
                "--workdir",
                &store,
            ],
            "'--roster [<PATH>]' cannot be used with",
        ),
    ] {
        let refused = mur(&args);
        assert_eq!(refused.status.code(), Some(2), "{args:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(refusal),
            "{args:?}: {refused:?}"
        );
    }
}
