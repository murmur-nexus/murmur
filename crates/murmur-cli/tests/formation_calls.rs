//! `call-member`: one formation member handing another a task over the roster's edge, the answer
//! coming back into the caller's same task.
//!
//! Every case launches a real formation with `mur run --roster` — real `mur` processes, real
//! doors, real HTTP between members — against a scripted model per member on loopback. No
//! provider key is used.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::door_capsule::{driver_home, end_turn, rpc, DRIVER_NAME, DRIVER_VERSION};
use common::formation::{
    assert_no_member_remains, launch_lock, reported_pids, session_dirs, Launcher, LAUNCH_LIMIT,
};
use common::{publish_to_store, read_whole_trace, ScriptedServer};
use serde_json::{json, Value};
use tempfile::TempDir;

/// A reply holding `calls`, one `call-member` tool use per `(member, task)`, in one response.
fn call_members(calls: &[(&str, &str)]) -> String {
    let content: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (member, task))| {
            json!({
                "type": "tool_use",
                "id": format!("toolu_call_{index}"),
                "name": "call-member",
                "input": {"member": member, "task": task},
            })
        })
        .collect();
    json!({
        "id": "msg_calls",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// One member's scripted model, and every request it has received, recorded on arrival.
struct Model {
    server: ScriptedServer,
    arrived: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Model {
    /// A model answering request `n` (from 1) with `answer(n)`.
    fn new(answer: impl Fn(usize) -> String + Send + 'static) -> Self {
        let arrived = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (counter, record) = (Arc::clone(&arrived), Arc::clone(&seen));
        let server = ScriptedServer::start_answering(16, move |request| {
            record.lock().unwrap().push(request.clone());
            answer(counter.fetch_add(1, Ordering::SeqCst) + 1)
        });
        Self {
            server,
            arrived,
            seen,
        }
    }

    /// A callee's model: every request is held until `release` is sent or dropped, then
    /// answered with `reply`.
    fn held(reply: impl Into<String>) -> (Self, mpsc::Sender<()>) {
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let reply = reply.into();
        let model = Self::new(move |_| {
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(120));
            end_turn(1, &reply)
        });
        (model, release)
    }

    fn arrived(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    /// Every request received so far, answered or still held.
    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

/// One member of a roster under test.
struct Member {
    name: &'static str,
    entry: bool,
    /// `capabilities.network.allow`, when the member declares one.
    allow: Option<&'static str>,
    /// `inference.max_turns`, when the member declares one.
    max_turns: Option<u32>,
    model: Model,
}

/// A member manifest's body: the fixture driver against `endpoint`, an authenticated door that
/// serves peers unless it is the entry member, `allow` as its egress and `max_turns` as its turn
/// limit.
fn manifest(endpoint: &str, entry: bool, allow: Option<&str>, max_turns: Option<u32>) -> String {
    let lifecycle = if entry {
        "task_acceptance: single\n  after_task: exit"
    } else {
        "task_acceptance: queue\n  after_task: sleep"
    };
    let capabilities = allow
        .map(|allow| format!("capabilities:\n  network:\n    allow: [{allow}]\n"))
        .unwrap_or_default();
    let max_turns = max_turns
        .map(|turns| format!("  max_turns: {turns}\n"))
        .unwrap_or_default();
    let exports = if entry {
        ""
    } else {
        "exports:\n  peer_tasks:\n    accept: true\n"
    };
    format!(
        "artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
         {capabilities}\
         lifecycle:\n  {lifecycle}\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n\
         {max_turns}\
         {exports}\
         network:\n  authentication:\n    scheme: bearer\n"
    )
}

/// A project holding a roster whose entry member is `lead`, each member published into the
/// project store, and a scratch `HOME` holding the driver.
struct Project {
    dir: TempDir,
    home: TempDir,
    members: Vec<Member>,
}

impl Project {
    fn new(members: Vec<Member>, reachability: &str) -> Self {
        Self::with_manifests(members, reachability, |member| {
            manifest(
                &member.model.server.endpoint,
                member.entry,
                member.allow,
                member.max_turns,
            )
        })
    }

    /// [`Self::new`], each member's manifest body written by `body`.
    fn with_manifests(
        members: Vec<Member>,
        reachability: &str,
        body: impl Fn(&Member) -> String,
    ) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("formation-calls-")
            .tempdir()
            .unwrap();
        let home = driver_home();
        let store = dir.path().join(".murmur").join("artifacts");
        let mut roster = String::from("roster_version: 1\nmembers:\n");
        for member in &members {
            let body = body(member);
            publish_to_store(&store, member.name, "0.1.0", "capsule", &body, None);
            roster.push_str(&format!(
                "  - name: {name}\n    capsule: {name}\n    version: 0.1.0\n{entry}",
                name = member.name,
                entry = if member.entry {
                    "    entry: true\n"
                } else {
                    ""
                },
            ));
        }
        roster.push_str(reachability);
        std::fs::write(dir.path().join("roster.yaml"), roster).unwrap();
        Self { dir, home, members }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn model(&self, name: &str) -> &Model {
        &self
            .members
            .iter()
            .find(|member| member.name == name)
            .unwrap_or_else(|| panic!("no member {name}"))
            .model
    }

    /// Block until `member`'s model has received `count` requests.
    fn await_requests(&self, member: &str, count: usize) {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        while self.model(member).arrived() < count {
            assert!(
                Instant::now() < deadline,
                "{member}'s model never received {count} requests"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `mur run --roster --json --task <task>`, started and read as it runs.
    fn launch(&self, task: &str, env: &[(&str, &str)]) -> Launcher {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(["run", "--roster", "--json", "--task", task])
            .current_dir(self.path())
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .env_remove(capsule_runtime::delegation_plane::DELEGATION_TIMEOUT_ENV)
            .env_remove("MURMUR_SPAWNER")
            .envs(env.iter().copied());
        Launcher::spawn(command)
    }

    /// The directory `member` of `formation` runs in.
    fn member_dir(&self, formation: &str, member: &str) -> PathBuf {
        self.home
            .path()
            .join(".murmur")
            .join("formations")
            .join(formation)
            .join(member)
    }

    /// Every record of the one session `member` of `formation` ran.
    fn trace_of(&self, formation: &str, member: &str) -> Vec<Value> {
        let root = if self.members.iter().any(|m| m.name == member && m.entry) {
            self.path().join(".murmur")
        } else {
            self.member_dir(formation, member).join(".murmur")
        };
        let sessions = session_dirs(&root);
        assert_eq!(sessions.len(), 1, "{member}: {sessions:?}");
        read_whole_trace(&sessions[0].join("trace.jsonl"))
    }

    /// The operator token the running record of `session_id` holds.
    fn door_token(&self, session_id: &str) -> String {
        common::formation::door_token(self.home.path(), session_id)
    }
}

fn records<'a>(trace: &'a [Value], kind: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|record| record["event_type"] == kind)
        .collect()
}

/// Block until `member`'s trace under `root` holds a `task_start`.
fn await_task_start(root: &Path, member: &str) {
    let deadline = Instant::now() + LAUNCH_LIMIT;
    loop {
        let started = session_dirs(root).iter().any(|session| {
            std::fs::read_to_string(session.join("trace.jsonl"))
                .is_ok_and(|trace| trace.contains("\"event_type\":\"task_start\""))
        });
        if started {
            return;
        }
        assert!(Instant::now() < deadline, "{member} never started a task");
        thread::sleep(Duration::from_millis(20));
    }
}

// ── Two callees at once ───────────────────────────────────────────────────────

/// Lead calls two members in one response. Both run at once, each with its task in a
/// `task.md` of its own while lead's stays in the project; both answers come back into lead's
/// task, each fenced under its member.
#[test]
fn two_callees_work_at_once_each_in_its_own_directory() {
    const LEAD_TASK: &str = "LEAD-TASK: ask worker and reviewer";
    const WORKER_TASK: &str = "WORKER-TASK: draft it";
    const REVIEWER_TASK: &str = "REVIEWER-TASK: review it";
    let lead = Model::new(|n| match n {
        1 => call_members(&[("worker", WORKER_TASK), ("reviewer", REVIEWER_TASK)]),
        _ => end_turn(n, "both answered"),
    });
    let (worker, release_worker) = Model::held("WORKER-ANSWER");
    let (reviewer, release_reviewer) = Model::held("REVIEWER-ANSWER");
    let project = Project::new(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: Some("localhost"),
                max_turns: None,
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
            Member {
                name: "reviewer",
                entry: false,
                allow: None,
                max_turns: None,
                model: reviewer,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker, reviewer]\n",
    );

    let _lock = launch_lock();
    let mut launcher = project.launch(LEAD_TASK, &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    for peer in formation["peers"].as_array().unwrap() {
        let name = peer["name"].as_str().unwrap();
        let workdir = project.member_dir(&formation_id, name);
        assert_eq!(peer["workdir"], workdir.display().to_string(), "{peer}");
    }

    // Both callees hold their task before either is answered.
    let worker_dir = project.member_dir(&formation_id, "worker");
    let reviewer_dir = project.member_dir(&formation_id, "reviewer");
    await_task_start(&worker_dir.join(".murmur"), "worker");
    await_task_start(&reviewer_dir.join(".murmur"), "reviewer");
    project.await_requests("worker", 1);
    project.await_requests("reviewer", 1);
    let read =
        |path: PathBuf| std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    assert_eq!(read(project.path().join("task.md")), LEAD_TASK);
    assert_eq!(read(worker_dir.join("task.md")), WORKER_TASK);
    assert_eq!(read(reviewer_dir.join("task.md")), REVIEWER_TASK);

    release_worker.send(()).unwrap();
    release_reviewer.send(()).unwrap();
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());
    assert_eq!(read(project.path().join("task.md")), LEAD_TASK);

    // Lead's last request carries the whole conversation: both answers, each under its member.
    let requests = project.model("lead").requests();
    let last = requests.last().unwrap().to_string();
    for (member, answer) in [("worker", "WORKER-ANSWER"), ("reviewer", "REVIEWER-ANSWER")] {
        let fenced = format!("<untrusted-content source=member:{member}>\\n{answer}");
        assert!(last.contains(&fenced), "{member}: {last}");
    }
    let lead_trace = project.trace_of(&formation_id, "lead");
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 2, "{lead_trace:?}");
    assert!(ends
        .iter()
        .all(|end| end["status"] == "completed" && end["delivered"] == true));
    assert_eq!(records(&lead_trace, "task_start").len(), 1);

    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

// ── A caller with no egress ───────────────────────────────────────────────────

/// A caller whose egress reaches no member's door is warned once at staging, and its call is
/// refused in the same turn naming the grant, never the door. The callee is never reached.
#[test]
fn a_caller_without_egress_is_warned_and_refused() {
    let lead = Model::new(|n| match n {
        1 => call_members(&[("worker", "anything")]),
        _ => end_turn(n, "could not call worker"),
    });
    let worker = Model::new(|n| end_turn(n, "unreachable"));
    let project = Project::new(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: None,
                max_turns: None,
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker]\n",
    );

    let _lock = launch_lock();
    let mut launcher = project.launch("call worker", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let door = formation["peers"][0]["url"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let stderr = launcher.stderr();
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("warning[W-RUN-008]"))
        .collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(
        warnings[0].contains("roster.yaml lets 'lead' call 'worker'"),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("declare \"localhost\""),
        "{}",
        warnings[0]
    );

    let requests = project.model("lead").requests();
    let raw = common::find_tool_result(&requests, "toolu_call_0").expect("a tool result");
    assert!(
        common::extract_result_text(&raw)
            .starts_with("<untrusted-content source=tool:call-member>\n"),
        "{raw}"
    );
    let result = common::tool_result_text(&requests, "toolu_call_0").expect("a tool result");
    assert!(result.contains("\"status\":\"failed\""), "{result}");
    assert!(result.contains("capabilities.network.allow"), "{result}");
    let port = door.rsplit(':').next().unwrap();
    assert!(
        !result.contains(&door) && !result.contains(&format!(":{port}")),
        "{result}"
    );

    let lead_session = &session_dirs(&project.path().join(".murmur"))[0];
    let bootstrap = std::fs::read_to_string(lead_session.join("logs/bootstrap.log")).unwrap();
    assert_eq!(
        bootstrap.matches("warning[W-RUN-008]").count(),
        1,
        "{bootstrap}"
    );

    let lead_trace = project.trace_of(&formation_id, "lead");
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 1, "{lead_trace:?}");
    assert_eq!(ends[0]["status"], "failed");
    assert!(ends[0].get("member_task_id").is_none(), "{}", ends[0]);
    assert!(records(&lead_trace, "member_call_start").is_empty());
    let worker_trace = project.trace_of(&formation_id, "worker");
    assert!(records(&worker_trace, "a2a_task_received").is_empty());
    assert_eq!(project.model("worker").arrived(), 0);

    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

// ── Timed-out and abandoned calls ─────────────────────────────────────────────

/// (a) A callee that does not answer within the shared bound gives lead a `timed_out` answer
/// that says the callee was not cancelled, and the callee's task is still working. (b) A launcher
/// stopped while lead waits on a call records the call `abandoned`, and nothing is left running.
#[test]
fn an_unanswered_call_times_out_and_an_ended_task_abandons_its_calls() {
    // (a)
    {
        let (release_lead, lead_released) = mpsc::channel::<()>();
        let lead_released = Mutex::new(lead_released);
        let lead = Model::new(move |n| match n {
            1 => call_members(&[("worker", "take your time")]),
            2 => end_turn(2, "waiting on worker"),
            _ => {
                let _ = lead_released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(120));
                end_turn(n, "worker timed out")
            }
        });
        let (worker, _never) = Model::held("too late");
        let project = Project::new(
            vec![
                Member {
                    name: "lead",
                    entry: true,
                    allow: Some("localhost"),
                    max_turns: None,
                    model: lead,
                },
                Member {
                    name: "worker",
                    entry: false,
                    allow: None,
                    max_turns: None,
                    model: worker,
                },
            ],
            "reachability:\n  - from: lead\n    to: [worker]\n",
        );

        let _lock = launch_lock();
        let mut launcher = project.launch(
            "call worker",
            &[(
                capsule_runtime::delegation_plane::DELEGATION_TIMEOUT_ENV,
                "2",
            )],
        );
        let formation = launcher.next_json().1;
        let formation_id = formation["formation_id"].as_str().unwrap().to_string();
        let peer = &formation["peers"][0];
        let readiness = launcher.next_json().1;

        // Lead is continued with the timed-out answer while worker is still working.
        project.await_requests("lead", 3);
        let requests = project.model("lead").requests();
        let continued = requests[2].to_string();
        assert!(
            continued.contains("<untrusted-content source=member:worker>"),
            "{continued}"
        );
        assert!(continued.contains("ended timed_out"), "{continued}");
        assert!(continued.contains("was not cancelled"), "{continued}");
        let worker_trace = read_whole_trace(
            &session_dirs(&project.member_dir(&formation_id, "worker").join(".murmur"))[0]
                .join("trace.jsonl"),
        );
        let task_id = records(&worker_trace, "a2a_task_received")[0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let token = project.door_token(peer["session_id"].as_str().unwrap());
        let addr = peer["url"].as_str().unwrap().trim_start_matches("http://");
        let task = rpc(addr, Some(&token), "tasks/get", json!({"id": task_id})).json();
        assert_eq!(task["result"]["status"]["state"], "working", "{task}");

        release_lead.send(()).unwrap();
        let status = launcher.wait();
        assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());
        let lead_trace = project.trace_of(&formation_id, "lead");
        let ends = records(&lead_trace, "member_call");
        assert_eq!(ends.len(), 1, "{lead_trace:?}");
        assert_eq!(ends[0]["status"], "timed_out");
        assert_eq!(ends[0]["delivered"], true);
        assert_no_member_remains(
            project.path(),
            &reported_pids(&formation, Some(&readiness)),
            Duration::from_secs(30),
        );
    }

    // (b)
    {
        let lead = Model::new(|n| match n {
            1 => call_members(&[("worker", "take your time")]),
            _ => end_turn(n, "waiting on worker"),
        });
        let (worker, _never) = Model::held("too late");
        let project = Project::new(
            vec![
                Member {
                    name: "lead",
                    entry: true,
                    allow: Some("localhost"),
                    max_turns: None,
                    model: lead,
                },
                Member {
                    name: "worker",
                    entry: false,
                    allow: None,
                    max_turns: None,
                    model: worker,
                },
            ],
            "reachability:\n  - from: lead\n    to: [worker]\n",
        );

        let _lock = launch_lock();
        let mut launcher = project.launch("call worker", &[]);
        let formation = launcher.next_json().1;
        let formation_id = formation["formation_id"].as_str().unwrap().to_string();
        let readiness = launcher.next_json().1;
        project.await_requests("lead", 2);
        project.await_requests("worker", 1);
        // Lead's attempt has ended; it is waiting on worker.
        thread::sleep(Duration::from_millis(500));
        launcher.signal(libc::SIGTERM);
        let status = launcher.wait();
        assert_eq!(status.code(), Some(143), "stderr:\n{}", launcher.stderr());
        assert_no_member_remains(
            project.path(),
            &reported_pids(&formation, Some(&readiness)),
            Duration::from_secs(30),
        );

        let lead_trace = project.trace_of(&formation_id, "lead");
        let ends = records(&lead_trace, "member_call");
        assert_eq!(ends.len(), 1, "{lead_trace:?}");
        assert_eq!(ends[0]["status"], "abandoned");
        assert_eq!(ends[0]["delivered"], false);
        // Every trace the formation wrote parses line by line.
        let _ = project.trace_of(&formation_id, "worker");
    }
}

/// A lead that ends its last allowed turn with a call still out does not wait for the answer it
/// has no turn to read: the call is recorded `abandoned`, undelivered, and the formation ends
/// without waiting out the bound for handed-off work.
#[test]
fn a_call_left_out_on_the_last_turn_is_abandoned_without_waiting() {
    let lead = Model::new(|n| match n {
        1 => call_members(&[("worker", "take your time")]),
        _ => end_turn(n, "out of turns"),
    });
    let (worker, _never) = Model::held("too late");
    let project = Project::new(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: Some("localhost"),
                max_turns: Some(2),
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker]\n",
    );

    let _lock = launch_lock();
    let mut launcher = project.launch(
        "call worker",
        &[(
            capsule_runtime::delegation_plane::DELEGATION_TIMEOUT_ENV,
            "600",
        )],
    );
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let ended = Instant::now();
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());
    assert!(
        ended.elapsed() < Duration::from_secs(60),
        "lead waited {:?} for an answer it had no turn to read",
        ended.elapsed()
    );
    assert_eq!(project.model("lead").arrived(), 2);

    let lead_trace = project.trace_of(&formation_id, "lead");
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 1, "{lead_trace:?}");
    assert_eq!(ends[0]["status"], "abandoned");
    assert_eq!(ends[0]["delivered"], false);
    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

// ── One pending call per member ───────────────────────────────────────────────

/// What the runtime says after a started call's fenced result: the answer is not in it, and the
/// next step is to end the turn.
const STARTED_NOTE: &str = "Its answer is not in this result and no tool fetches it: the runtime \
     adds it to this conversation after you end your turn. Unless you still have work to hand to a \
     different member, end your turn now by replying without calling a tool. Calling worker again \
     before its answer arrives is refused.";

/// The runtime's last line on a delivery once no call is left out.
const EVERY_CALL_ENDED: &str = "[call-member] Every call this task made has ended, and the \
     answers are above. Answer the task with them now; call a member again only to give it new \
     work.";

/// A two-member roster, `lead` calling `worker`, each on its own scripted model.
fn lead_and_worker(lead: Model, worker: Model) -> Project {
    Project::new(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: Some("localhost"),
                max_turns: None,
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker]\n",
    )
}

/// The `call-member` `tool_call` records of `trace`, in order.
fn call_member_tool_calls(trace: &[Value]) -> Vec<&Value> {
    records(trace, "tool_call")
        .into_iter()
        .filter(|record| record["tool_name"] == "call-member")
        .collect()
}

/// Lead calls worker, then calls worker again before the answer arrives. The second call is a
/// tool error that sends nothing and names the first call; the started result carries the
/// runtime's note after its fence; worker runs one task, and its answer still reaches lead with
/// the runtime's line that every call has ended.
#[test]
fn a_repeated_call_is_refused_and_the_answer_still_arrives() {
    const TASK: &str = "Write one line about the sea.";
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let lead = Model::new(|n| match n {
        1 => common::tool_use_response(
            "toolu_first",
            "call-member",
            json!({"member": "worker", "task": TASK}),
        ),
        2 => common::tool_use_response(
            "toolu_repeat",
            "call-member",
            json!({"member": "worker", "task": TASK}),
        ),
        3 => end_turn(3, "waiting"),
        _ => end_turn(n, "worker answered"),
    });
    let answer = format!("WORKER-{nonce}");
    let (worker, release_worker) = Model::held(answer.clone());
    let project = lead_and_worker(lead, worker);

    let _lock = launch_lock();
    let mut launcher = project.launch("ask worker", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    // Worker answers only once lead has been refused, so the refusal is the not-yet-answered one.
    project.await_requests("lead", 3);
    release_worker.send(()).unwrap();
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let lead_trace = project.trace_of(&formation_id, "lead");
    let starts = records(&lead_trace, "member_call_start");
    assert_eq!(starts.len(), 1, "{lead_trace:?}");
    let call_id = starts[0]["call_id"].as_str().unwrap();
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 1, "{lead_trace:?}");
    assert_eq!(ends[0]["status"], "completed");
    assert_eq!(ends[0]["delivered"], true);
    let tool_calls = call_member_tool_calls(&lead_trace);
    assert_eq!(tool_calls.len(), 2, "{tool_calls:?}");
    assert_ne!(tool_calls[0]["status"], "error", "{}", tool_calls[0]);
    assert_eq!(tool_calls[1]["status"], "error", "{}", tool_calls[1]);
    assert_eq!(records(&lead_trace, "task_end")[0]["exit_status"], "ok");

    let requests = project.model("lead").requests();
    assert_eq!(requests.len(), 4, "{requests:?}");

    // Request 2: the started JSON inside the fence, the note after its closing marker.
    let started = common::find_tool_result(&requests[1..2], "toolu_first").unwrap();
    let started = common::extract_result_text(&started);
    let (fenced, note) = started.split_once("\n</untrusted-content>\n").unwrap();
    let fenced = fenced
        .strip_prefix("<untrusted-content source=tool:call-member>\n")
        .unwrap_or_else(|| panic!("{started}"));
    let data: Value = serde_json::from_str(fenced).unwrap();
    assert_eq!(data["status"], "started", "{data}");
    assert_eq!(data["call_id"], call_id, "{data}");
    assert!(!fenced.contains("[call-member]"), "{started}");
    assert_eq!(
        note,
        format!("[call-member] worker is now working on call {call_id}. {STARTED_NOTE}")
    );

    // Request 3: the repeat, a tool error with no fence, naming the first call. The runtime
    // marks it `is_error: true`; the Anthropic driver fixture does not carry that flag onto the
    // wire block, so the error is asserted on the `tool_call` record above.
    let repeat = common::find_tool_result(&requests[2..3], "toolu_repeat").unwrap();
    let refusal = common::extract_result_text(&repeat);
    assert_eq!(
        refusal,
        format!(
            "worker has not yet answered call {call_id} from this task, so nothing was sent. Its \
             answer reaches you only after you end your turn: end your turn now by replying \
             without calling a tool. Once that answer has arrived you may call worker again with \
             new work."
        )
    );

    // Request 4: worker's answer under its member, then the runtime's line.
    let delivered = common::last_user_text(&requests[3]);
    assert!(
        delivered.contains(&format!(
            "<untrusted-content source=member:worker>\n{answer}"
        )),
        "{delivered}"
    );
    assert!(delivered.ends_with(EVERY_CALL_ENDED), "{delivered}");

    let worker_trace = project.trace_of(&formation_id, "worker");
    assert_eq!(records(&worker_trace, "a2a_task_received").len(), 1);
    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

/// Once worker's answer has been delivered, a second call to worker is new work: it starts as a
/// call of its own and is answered like the first.
#[test]
fn a_follow_up_after_the_answer_is_a_new_call() {
    let lead = Model::new(|n| match n {
        1 => common::tool_use_response(
            "toolu_first",
            "call-member",
            json!({"member": "worker", "task": "Draft a line."}),
        ),
        2 => end_turn(2, "waiting"),
        3 => common::tool_use_response(
            "toolu_second",
            "call-member",
            json!({"member": "worker", "task": "Now shorten it."}),
        ),
        4 => end_turn(4, "waiting again"),
        _ => end_turn(n, "done"),
    });
    let worker = Model::new(|n| end_turn(n, &format!("WORKER-ANSWER-{n}")));
    let project = lead_and_worker(lead, worker);

    let _lock = launch_lock();
    let mut launcher = project.launch("ask worker twice", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let lead_trace = project.trace_of(&formation_id, "lead");
    let starts = records(&lead_trace, "member_call_start");
    assert_eq!(starts.len(), 2, "{lead_trace:?}");
    assert_ne!(starts[0]["call_id"], starts[1]["call_id"]);
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 2, "{lead_trace:?}");
    assert!(ends
        .iter()
        .all(|end| end["status"] == "completed" && end["delivered"] == true));
    assert!(call_member_tool_calls(&lead_trace)
        .iter()
        .all(|call| call["status"] != "error"));
    assert_eq!(project.model("lead").arrived(), 5);
    let worker_trace = project.trace_of(&formation_id, "worker");
    assert_eq!(records(&worker_trace, "a2a_task_received").len(), 2);
    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

/// The `turn` of every agent-loop `inference` record of `trace`, in file order.
fn agent_loop_turns(trace: &[Value]) -> Vec<u64> {
    records(trace, "inference")
        .into_iter()
        .filter(|record| record.get("origin").is_none_or(Value::is_null))
        .map(|record| record["turn"].as_u64().unwrap())
        .collect()
}

/// The lines of `output` from the section headed `── <name>` up to the blank line ending it.
fn section<'a>(output: &'a str, name: &str) -> Vec<&'a str> {
    output
        .lines()
        .skip_while(|line| !line.starts_with(&format!("── {name} ")))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .collect()
}

/// A lead that calls worker, ends its turn to wait, and continues with the answer numbers that
/// continuation's turn 2: the trace, `mur trace show`, `mur trace steps` and `--turn` all read
/// turns 0, 1 and 2.
#[test]
fn a_continued_lead_numbers_its_turns_straight_through() {
    let lead = Model::new(|n| match n {
        1 => common::tool_use_response(
            "toolu_first",
            "call-member",
            json!({"member": "worker", "task": "Draft a line."}),
        ),
        2 => end_turn(2, "waiting"),
        _ => end_turn(n, "done"),
    });
    let worker = Model::new(|n| end_turn(n, "WORKER-ANSWER"));
    let project = Project::with_manifests(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: Some("localhost"),
                max_turns: None,
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker]\n",
        |member| {
            let body = manifest(
                &member.model.server.endpoint,
                member.entry,
                member.allow,
                member.max_turns,
            );
            if member.entry {
                body + "trace:\n  capture: content\n"
            } else {
                body
            }
        },
    );

    let _lock = launch_lock();
    let mut launcher = project.launch("ask worker", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let lead_trace = project.trace_of(&formation_id, "lead");
    assert_eq!(agent_loop_turns(&lead_trace), [0, 1, 2], "{lead_trace:?}");
    let calls = call_member_tool_calls(&lead_trace);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["turn"], 0);
    assert_eq!(records(&lead_trace, "task_end")[0]["turns"], 3);

    let workdir = project.path().join(".murmur");
    let lead_session = &session_dirs(&workdir)[0];
    let session_id = lead_session.file_name().unwrap().to_str().unwrap();
    let mur = |args: &[&str]| {
        let output = Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .arg("trace")
            .args(args)
            .args([session_id, "--workdir", workdir.to_str().unwrap()])
            .env("HOME", project.home.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        output.stdout
    };

    let show = String::from_utf8(mur(&["show"])).unwrap();
    let wire_turns: Vec<&str> = section(&show, "Wire")
        .into_iter()
        .filter_map(|line| line.split_once("  system ").map(|(turn, _)| turn))
        .collect();
    assert_eq!(wire_turns, ["turn 0", "turn 1", "turn 2"], "{show}");
    let tool_call_rows: Vec<&str> = section(&show, "Tool calls")
        .into_iter()
        .filter_map(|line| line.trim_start().strip_prefix("turn "))
        .filter_map(|row| row.split("  ").next())
        .collect();
    assert_eq!(tool_call_rows, ["0", "1", "2"], "{show}");

    let steps = String::from_utf8(mur(&["steps"])).unwrap();
    for row in [
        "turn 0  tool_call  call-member",
        "turn 1  end_turn",
        "turn 2  end_turn",
    ] {
        assert!(steps.contains(row), "{row}: {steps}");
    }

    let response = mur(&["show", "--body", "response", "--turn", "2"]);
    let third = &records(&lead_trace, "inference")
        .into_iter()
        .filter(|record| record.get("origin").is_none_or(Value::is_null))
        .nth(2)
        .unwrap()["response_sha"];
    assert_eq!(&json!(murmur_artifact::sha256_hex(&response)), third);

    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

/// Two calls to worker in one response start once: the first is started, the second refused,
/// and worker runs only the first task.
#[test]
fn one_member_twice_in_one_response_starts_once() {
    let lead = Model::new(|n| match n {
        1 => call_members(&[("worker", "TASK-A"), ("worker", "TASK-B")]),
        2 => end_turn(2, "waiting"),
        _ => end_turn(n, "done"),
    });
    let worker = Model::new(|n| end_turn(n, "WORKER-ANSWER"));
    let project = lead_and_worker(lead, worker);

    let _lock = launch_lock();
    let mut launcher = project.launch("ask worker", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let requests = project.model("lead").requests();
    let first = common::find_tool_result(&requests, "toolu_call_0").unwrap();
    let first = common::extract_result_text(&first);
    assert!(first.contains("\"status\":\"started\""), "{first}");
    assert!(first.contains("is now working on call"), "{first}");
    let second = common::find_tool_result(&requests, "toolu_call_1").unwrap();
    let lead_trace = project.trace_of(&formation_id, "lead");
    let tool_calls = call_member_tool_calls(&lead_trace);
    assert_eq!(tool_calls.len(), 2, "{tool_calls:?}");
    assert_ne!(tool_calls[0]["status"], "error", "{}", tool_calls[0]);
    assert_eq!(tool_calls[1]["status"], "error", "{}", tool_calls[1]);
    let starts = records(&lead_trace, "member_call_start");
    assert_eq!(starts.len(), 1, "{lead_trace:?}");
    let call_id = starts[0]["call_id"].as_str().unwrap();
    let refusal = common::extract_result_text(&second);
    assert!(
        refusal.starts_with(&format!(
            "worker has not yet answered call {call_id} from this task, so nothing was sent."
        )) || refusal.starts_with(&format!("worker has already answered call {call_id},")),
        "{refusal}"
    );
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 1, "{lead_trace:?}");
    assert_eq!(ends[0]["delivered"], true);

    let worker_requests = project.model("worker").requests();
    assert_eq!(worker_requests.len(), 1, "{worker_requests:?}");
    let told = worker_requests[0].to_string();
    assert!(
        told.contains("TASK-A") && !told.contains("TASK-B"),
        "{told}"
    );
    let worker_trace = project.trace_of(&formation_id, "worker");
    assert_eq!(records(&worker_trace, "a2a_task_received").len(), 1);
    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

// ── A process lead ────────────────────────────────────────────────────────────

/// The fixture process driver, run by every `transport: process` lead here.
const PROCESS_DRIVER: &str = "fixture-process-driver";

/// A `transport: process` lead whose harness calls worker through the bridged `call-member`
/// and ends its turn. The bridged result carries the runtime's note after its fence; the answer
/// reaches the harness as the prompt of a resume of the same session, ending with the runtime's
/// line that every call has ended.
#[test]
fn a_process_lead_gets_the_answer_in_its_resumed_session() {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let answer = format!("WORKER-{nonce}");
    let worker = Model::new({
        let answer = answer.clone();
        move |n| end_turn(n, &answer)
    });
    // Lead's inference runs in the harness; its scripted model is never asked.
    let lead = Model::new(|n| end_turn(n, "unused"));
    let project = Project::with_manifests(
        vec![
            Member {
                name: "lead",
                entry: true,
                allow: Some("localhost"),
                max_turns: None,
                model: lead,
            },
            Member {
                name: "worker",
                entry: false,
                allow: None,
                max_turns: None,
                model: worker,
            },
        ],
        "reachability:\n  - from: lead\n    to: [worker]\n",
        |member| {
            if member.entry {
                format!(
                    "artifacts:\n  - name: {PROCESS_DRIVER}\n    version: {DRIVER_VERSION}\n    \
                     runtime: driver\n\
                     capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n  \
                     network:\n    allow: [localhost]\n\
                     lifecycle:\n  task_acceptance: single\n  after_task: exit\n\
                     inference:\n  transport: process\n  driver:\n    artifact: {PROCESS_DRIVER}\n\
                     network:\n  authentication:\n    scheme: bearer\n"
                )
            } else {
                manifest(
                    &member.model.server.endpoint,
                    member.entry,
                    member.allow,
                    member.max_turns,
                )
            }
        },
    );

    // The process driver beside the http one, and the fake harness as `fixture-cli` on a `PATH`
    // of lead's own.
    let artifacts = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        PROCESS_DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    common::publish_local(&project.home, &driver).success();
    let bin_dir = project.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let harness = bin_dir.join("fixture-cli");
    std::fs::copy(
        common::fixture_path("process-driver/fake-harness"),
        &harness,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&harness).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&harness, perms).unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
    );

    let _lock = launch_lock();
    let mut launcher = project.launch(
        "Get worker's token.",
        &[
            ("PATH", path.as_str()),
            ("FIXTURE_HARNESS_PROFILE", "member-call"),
        ],
    );
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let lead_trace = project.trace_of(&formation_id, "lead");
    let harness_starts = records(&lead_trace, "harness_start");
    assert_eq!(harness_starts.len(), 2, "{harness_starts:?}");
    assert_eq!(harness_starts[0]["session_mode"], "new");
    assert_eq!(harness_starts[1]["session_mode"], "resume");
    assert_eq!(
        harness_starts[0]["harness_session_id"],
        harness_starts[1]["harness_session_id"]
    );
    let starts = records(&lead_trace, "member_call_start");
    assert_eq!(starts.len(), 1, "{lead_trace:?}");
    let call_id = starts[0]["call_id"].as_str().unwrap();
    let ends = records(&lead_trace, "member_call");
    assert_eq!(ends.len(), 1, "{lead_trace:?}");
    assert_eq!(ends[0]["status"], "completed");
    assert_eq!(ends[0]["delivered"], true);
    assert_eq!(records(&lead_trace, "task_end")[0]["exit_status"], "ok");
    // The resumed run numbers its turns on from the first run's: one sequence, no repeat.
    let turns = agent_loop_turns(&lead_trace);
    assert_eq!(
        turns,
        (0..turns.len() as u64).collect::<Vec<_>>(),
        "{turns:?}"
    );
    let resumed_at = lead_trace
        .iter()
        .position(|record| record == harness_starts[1])
        .unwrap();
    let before = agent_loop_turns(&lead_trace[..resumed_at]);
    let after = agent_loop_turns(&lead_trace[resumed_at..]);
    assert!(
        !before.is_empty() && after.first() > before.iter().max(),
        "{before:?} then {after:?}"
    );

    // What the harness read back from the bridge: the started JSON fenced, the note after.
    let bridged = std::fs::read_to_string(
        project
            .home
            .path()
            .join("fake-harness-sessions/call-member-answer"),
    )
    .unwrap();
    let bridged: Value = serde_json::from_str(&bridged).unwrap();
    let text = bridged
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{bridged}"));
    let (fenced, note) = text.split_once("\n</untrusted-content>\n").unwrap();
    let fenced = fenced
        .strip_prefix("<untrusted-content source=tool:call-member>\n")
        .unwrap_or_else(|| panic!("{text}"));
    let data: Value = serde_json::from_str(fenced).unwrap();
    assert_eq!(data["status"], "started", "{data}");
    assert_eq!(data["call_id"], call_id, "{data}");
    assert_eq!(
        note,
        format!("[call-member] worker is now working on call {call_id}. {STARTED_NOTE}")
    );

    // What the resumed harness was told, as it answered with it.
    let lead_session = &session_dirs(&project.path().join(".murmur"))[0];
    let result = std::fs::read_to_string(lead_session.join("out/result.txt")).unwrap();
    for said in [
        format!("[call-member] call {call_id} to worker ended completed:"),
        "<untrusted-content source=member:worker>".to_string(),
        answer.clone(),
        EVERY_CALL_ENDED.to_string(),
    ] {
        assert!(result.contains(&said), "{said}: {result}");
    }

    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}

// ── Who called whom ───────────────────────────────────────────────────────────

/// Block until some session under `root` has a trace line containing every one of `needles`.
fn await_trace_line(root: &Path, needles: &[&str]) {
    let deadline = Instant::now() + LAUNCH_LIMIT;
    loop {
        let found = session_dirs(root).iter().any(|session| {
            std::fs::read_to_string(session.join("trace.jsonl")).is_ok_and(|trace| {
                trace
                    .lines()
                    .any(|line| needles.iter().all(|needle| line.contains(needle)))
            })
        });
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no line under {root:?} holds {needles:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A two-tier formation: `chief` calls two leads in one response, and each lead calls its four
/// workers in one response. `worker-b4`'s door is full of operator tasks when `lead-b` calls it,
/// so that call is refused. `mur trace show <frm>`, run from the roster's directory, lists every
/// member by roster name and every call nested under the call that handed over its task, the
/// refused one `rejected` and never held, and no row for the operator's tasks.
#[test]
fn two_tier_formation_trace_names_every_call() {
    const A_WORKERS: [&str; 4] = ["worker-a1", "worker-a2", "worker-a3", "worker-a4"];
    const B_WORKERS: [&str; 4] = ["worker-b1", "worker-b2", "worker-b3", "worker-b4"];
    let (open_gate, gate) = mpsc::channel::<()>();
    let gate = Mutex::new(gate);
    let chief = Model::new(move |n| match n {
        1 => {
            let _ = gate.lock().unwrap().recv_timeout(LAUNCH_LIMIT);
            call_members(&[("lead-a", "LEAD-A-TASK"), ("lead-b", "LEAD-B-TASK")])
        }
        _ => end_turn(n, "both leads answered"),
    });
    let lead = |workers: [&'static str; 4]| {
        Model::new(move |n| match n {
            1 => call_members(&workers.map(|worker| (worker, "WORKER-TASK"))),
            _ => end_turn(n, "my workers answered"),
        })
    };
    let (busy, release_busy) = Model::held("BUSY-ANSWER");
    let mut members = vec![
        Member {
            name: "chief",
            entry: true,
            allow: Some("localhost"),
            max_turns: None,
            model: chief,
        },
        Member {
            name: "lead-a",
            entry: false,
            allow: Some("localhost"),
            max_turns: None,
            model: lead(A_WORKERS),
        },
        Member {
            name: "lead-b",
            entry: false,
            allow: Some("localhost"),
            max_turns: None,
            model: lead(B_WORKERS),
        },
    ];
    for name in A_WORKERS.iter().chain(&B_WORKERS[..3]) {
        members.push(Member {
            name,
            entry: false,
            allow: None,
            max_turns: None,
            model: Model::new(|n| end_turn(n, "WORKER-ANSWER")),
        });
    }
    members.push(Member {
        name: "worker-b4",
        entry: false,
        allow: None,
        max_turns: None,
        model: busy,
    });
    let project = Project::new(
        members,
        &format!(
            "reachability:\n  - from: chief\n    to: [lead-a, lead-b]\n  - from: lead-a\n    \
             to: [{}]\n  - from: lead-b\n    to: [{}]\n",
            A_WORKERS.join(", "),
            B_WORKERS.join(", ")
        ),
    );

    let _lock = launch_lock();
    let mut launcher = project.launch("CHIEF-TASK", &[]);
    let formation = launcher.next_json().1;
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = launcher.next_json().1;

    // Fill worker-b4's door with operator tasks, its model holding the first, until it refuses.
    let peer = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["name"] == "worker-b4")
        .unwrap();
    let token = project.door_token(peer["session_id"].as_str().unwrap());
    let addr = peer["url"].as_str().unwrap().trim_start_matches("http://");
    let mut sent = 0;
    loop {
        sent += 1;
        assert!(sent <= 8, "worker-b4's door never refused a task");
        let answer = rpc(
            addr,
            Some(&token),
            "message/send",
            common::door_capsule::message(&format!("m-op-{sent}"), "OPERATOR-TASK"),
        )
        .json();
        if answer["result"]["status"]["state"] == "rejected" {
            break;
        }
        if sent == 1 {
            project.await_requests("worker-b4", 1);
        }
    }

    open_gate.send(()).unwrap();
    let lead_b_root = project.member_dir(&formation_id, "lead-b").join(".murmur");
    await_trace_line(
        &lead_b_root,
        &["\"event_type\":\"member_call\"", "\"member\":\"worker-b4\""],
    );
    drop(release_busy);
    let status = launcher.wait();
    assert_eq!(status.code(), Some(0), "stderr:\n{}", launcher.stderr());

    let lead_b = project.trace_of(&formation_id, "lead-b");
    let refused: Vec<&Value> = records(&lead_b, "member_call")
        .into_iter()
        .filter(|record| record["member"] == "worker-b4")
        .collect();
    assert_eq!(refused.len(), 1, "{lead_b:?}");
    assert_eq!(refused[0]["status"], "rejected", "{}", refused[0]);
    assert!(refused[0].get("member_task_id").is_none(), "{}", refused[0]);
    println!("lead-b's member_call for worker-b4:\n{}", refused[0]);

    let mut trace_bytes = 0;
    let mut roots = vec![project.path().join(".murmur")];
    for member in project.members.iter().filter(|member| !member.entry) {
        roots.push(
            project
                .member_dir(&formation_id, member.name)
                .join(".murmur"),
        );
    }
    for root in &roots {
        for session in session_dirs(root) {
            trace_bytes += std::fs::metadata(session.join("trace.jsonl"))
                .unwrap()
                .len();
        }
    }

    let began = Instant::now();
    let shown = Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .args(["trace", "show", &formation_id])
        .current_dir(project.path())
        .env("HOME", project.home.path())
        .output()
        .unwrap();
    let took = began.elapsed();
    let stdout = String::from_utf8_lossy(&shown.stdout).to_string();
    assert!(
        shown.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    println!("{stdout}");
    println!(
        "cost: {} member traces, {trace_bytes} bytes; mur trace show took {} ms",
        roots.len(),
        took.as_millis()
    );

    let lines: Vec<&str> = stdout.lines().collect();
    let mut named: Vec<&str> = lines
        .iter()
        .filter(|line| line.starts_with("ses_"))
        .map(|line| line.split_whitespace().nth(1).unwrap())
        .collect();
    named.sort();
    let mut expected: Vec<&str> = project.members.iter().map(|member| member.name).collect();
    expected.sort();
    assert_eq!(named, expected, "{stdout}");
    let chief_row = lines
        .iter()
        .find(|line| line.starts_with("ses_") && line.split_whitespace().nth(1) == Some("chief"))
        .unwrap();
    assert!(chief_row.ends_with("  may call lead-a, lead-b"), "{stdout}");

    // Each call's start, as its caller recorded it: the `member_call_start`, or for a call its
    // callee never held, the `member_call` less its duration.
    let mut started: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for caller in ["chief", "lead-a", "lead-b"] {
        for record in project.trace_of(&formation_id, caller) {
            let call_id = record["call_id"].as_str().unwrap_or_default().to_string();
            let at = record["timestamp"].as_u64().unwrap_or_default();
            match record["event_type"].as_str() {
                Some("member_call_start") => {
                    started.insert(call_id, at);
                }
                Some("member_call") => {
                    let took = record["duration_ms"].as_u64().unwrap_or_default();
                    started.entry(call_id).or_insert(at - took);
                }
                _ => {}
            }
        }
    }
    let start_of = |line: &str| -> u64 {
        let call_id = line.split_whitespace().next().unwrap();
        *started
            .get(call_id)
            .unwrap_or_else(|| panic!("no recorded start for {call_id}"))
    };

    let at = lines
        .iter()
        .position(|line| *line == "calls:      10")
        .unwrap_or_else(|| panic!("no `calls:      10` line:\n{stdout}"));
    let rows = &lines[at + 1..];
    assert_eq!(rows.len(), 10, "{stdout}");
    // `<call id>  <caller> → <callee>  …`, at its depth.
    let row = |line: &str| -> (usize, String, String) {
        let depth = (line.len() - line.trim_start().len()) / 2;
        let words: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(words[2], "→", "{line}");
        (depth, words[1].to_string(), words[3].to_string())
    };
    for tier in [&rows[..5], &rows[5..]] {
        let (depth, caller, lead) = row(tier[0]);
        assert_eq!((depth, caller.as_str()), (0, "chief"), "{stdout}");
        let workers = if lead == "lead-a" {
            A_WORKERS
        } else {
            B_WORKERS
        };
        let mut called: Vec<String> = tier[1..]
            .iter()
            .map(|line| {
                let (depth, caller, worker) = row(line);
                assert_eq!((depth, caller.as_str()), (1, lead.as_str()), "{stdout}");
                worker
            })
            .collect();
        let starts: Vec<u64> = tier[1..].iter().map(|line| start_of(line)).collect();
        assert!(starts.is_sorted(), "{starts:?}\n{stdout}");
        called.sort();
        assert_eq!(called, workers, "{stdout}");
    }
    assert_ne!(row(rows[0]).2, row(rows[5]).2, "{stdout}");
    assert!(start_of(rows[0]) <= start_of(rows[5]), "{stdout}");
    let completed = rows
        .iter()
        .filter(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            words[4] == "completed" && words[6] == "delivered" && words.len() == 7
        })
        .count();
    assert_eq!(completed, 9, "{stdout}");
    let refused_rows: Vec<&&str> = rows
        .iter()
        .filter(|line| line.contains("→ worker-b4"))
        .collect();
    assert_eq!(refused_rows.len(), 1, "{stdout}");
    let words: Vec<&str> = refused_rows[0].split_whitespace().collect();
    assert_eq!(words[4], "rejected", "{stdout}");
    assert!(
        refused_rows[0].ends_with("never held by worker-b4"),
        "{stdout}"
    );

    assert_no_member_remains(
        project.path(),
        &reported_pids(&formation, Some(&readiness)),
        Duration::from_secs(30),
    );
}
