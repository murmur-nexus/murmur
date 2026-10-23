//! A launch's own task — `mur run --task` — is a task on the capsule's door from the moment the
//! door is announced: `ListTasks` and `GetTask` see it, `SubscribeToTask` follows it to its end,
//! `CancelTask` reaches it, and a `single` door refuses a second task as busy while it runs. Its
//! `task_start` still says `source: "task_md"`, and it has no `a2a_task_received`, having never
//! crossed the door.
//!
//! Each test runs `mur run` as its own process against a provider that holds the first model
//! request until the test releases it, so the task is observably running.

#[path = "common/mod.rs"]
mod common;

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use common::door_capsule::{
    agent_project, driver_home, end_turn, message, mur, returning_immediately, rpc, trace_events,
    AUTHENTICATION_YAML,
};
use common::stream;
use serde_json::{json, Value};
use tempfile::TempDir;

/// The task every launch here is given.
const TASK: &str = "say hello through the door";

/// A provider's first request, held until [`Held::release`].
struct Held {
    release: mpsc::Sender<()>,
    arrived: mpsc::Receiver<()>,
}

impl Held {
    /// Blocks until the held request has arrived: the launch task is then running.
    fn wait_arrived(&self) {
        self.arrived
            .recv_timeout(Duration::from_secs(120))
            .expect("the provider never saw the launch task's request");
    }

    /// Lets the held request be answered.
    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

/// A provider whose first request is held until [`Held::release`], or until the [`Held`] is
/// dropped, and is then answered with an `end_turn` saying `done`.
fn held_provider() -> (common::ScriptedServer, Held) {
    let (release, released) = mpsc::channel::<()>();
    let (arrive, arrived) = mpsc::channel::<()>();
    let server = common::ScriptedServer::start_answering(1, move |_| {
        let _ = arrive.send(());
        let _ = released.recv_timeout(Duration::from_secs(120));
        end_turn(1, "done")
    });
    (server, Held { release, arrived })
}

/// One `mur run --json --task` process, with every line it writes to stdout kept.
struct Launch {
    child: Child,
    /// The readiness line.
    ready: Value,
    /// Everything on stdout after the readiness line, once the process has closed it.
    rest: Option<thread::JoinHandle<Vec<String>>>,
    stderr: Option<thread::JoinHandle<String>>,
    home: PathBuf,
}

impl Launch {
    /// `mur run --manifest <manifest> --json --task <TASK>` under `home`, blocking until the door
    /// is announced.
    fn start(home: &Path, manifest: &Path) -> Self {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .args(["run", "--manifest"])
            .arg(manifest)
            .args(["--json", "--task", TASK])
            .current_dir(manifest.parent().unwrap())
            .env("HOME", home)
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("mur run should start");
        let stderr_pipe = child.stderr.take().unwrap();
        let stderr = thread::spawn(move || {
            let mut all = String::new();
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
                all.push_str(&line);
                all.push('\n');
            }
            all
        });
        let (ready_tx, ready_rx) = mpsc::channel::<String>();
        let stdout = child.stdout.take().unwrap();
        let rest = thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines().map_while(Result::ok);
            if let Some(first) = lines.next() {
                let _ = ready_tx.send(first);
            }
            lines.collect()
        });
        let ready = ready_rx
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| panic!("the door was never announced"));
        let ready: Value = serde_json::from_str(&ready)
            .unwrap_or_else(|e| panic!("the readiness line is not JSON ({e}): {ready}"));
        Launch {
            child,
            ready,
            rest: Some(rest),
            stderr: Some(stderr),
            home: home.to_path_buf(),
        }
    }

    fn url(&self) -> String {
        self.ready["url"].as_str().unwrap().to_string()
    }

    fn session_id(&self) -> String {
        self.ready["session_id"].as_str().unwrap().to_string()
    }

    fn records_dir(&self) -> PathBuf {
        PathBuf::from(self.ready["records_dir"].as_str().unwrap())
    }

    /// The session's operator token, as `mur token <session>` prints it.
    fn token(&self) -> String {
        let output = mur(&self.home, &[])
            .args(["token", &self.session_id()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mur token failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let response = rpc(&self.url(), Some(token), method, params);
        assert_eq!(response.status, 200, "{method}: {response:?}");
        response.json()
    }

    /// Waits for the process to exit, and returns its exit code, the stdout lines after the
    /// readiness line, and its stderr.
    fn finish(mut self) -> (Option<i32>, Vec<String>, String) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "mur run never exited");
            thread::sleep(Duration::from_millis(50));
        };
        let rest = self.rest.take().unwrap().join().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (status.code(), rest, stderr)
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An authenticated agent capsule with no lifecycle block, against `endpoint`.
fn authenticated_project(endpoint: &str, name: &str) -> TempDir {
    agent_project(endpoint, name, "", AUTHENTICATION_YAML)
}

/// The tasks `ListTasks` answers with.
fn listed(answer: &Value) -> Vec<Value> {
    answer["result"]["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("no tasks in {answer}"))
        .clone()
}

/// Every record of type `event_type` in the session's trace.
fn records(records_dir: &Path, event_type: &str) -> Vec<Value> {
    trace_events(records_dir)
        .into_iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// S1: while the launch task's model request is held, `ListTasks` lists exactly it, working,
/// under the launch's context; `GetTask` agrees; a `SubscribeToTask` stream opened then ends with
/// its completion. The trace names the same task, still `task_md` from the user, and nothing
/// says it arrived over the door. `mur run --json` prints its readiness line and nothing else,
/// and exits 0.
#[test]
fn a_launch_task_is_a_task_on_the_door_while_it_runs() {
    let home = driver_home();
    let (provider, held) = held_provider();
    let project = authenticated_project(&provider.endpoint, "launch-task-agent");
    let launch = Launch::start(home.path(), &project.path().join("murmur.yaml"));
    let token = launch.token();
    held.wait_arrived();

    let tasks = listed(&launch.rpc(&token, "ListTasks", json!({})));
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let task = &tasks[0];
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING", "{task}");
    let task_id = task["id"].as_str().unwrap().to_string();
    let start = records(&launch.records_dir(), "task_start");
    assert_eq!(start.len(), 1, "{start:?}");
    assert_eq!(start[0]["task_id"], task_id.as_str());
    assert_eq!(task["contextId"], start[0]["context_id"], "{task}");
    assert_eq!(start[0]["source"], "task_md");
    assert_eq!(start[0]["origin"], "user");

    let got = launch.rpc(&token, "GetTask", json!({"id": task_id}));
    assert_eq!(got["result"]["id"], task_id.as_str(), "{got}");
    assert_eq!(got["result"]["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(got["result"]["contextId"], task["contextId"]);

    let (url, subscriber_token, followed) = (launch.url(), token.clone(), task_id.clone());
    let subscription = thread::spawn(move || {
        stream::subscribe(&url, Some(&subscriber_token), &followed, false, None)
    });
    // The stream is opened while the request is still held.
    thread::sleep(Duration::from_secs(1));
    held.release();
    let (response, events) = subscription.join().unwrap();
    assert_eq!(response.status, 200, "{response:?}");
    let last = events
        .last()
        .unwrap_or_else(|| panic!("an empty stream: {}", response.body));
    assert!(last.is_terminal_status(), "{events:?}");
    assert_eq!(last.task_id(), Some(task_id.as_str()), "{events:?}");
    assert_eq!(last.state(), Some("TASK_STATE_COMPLETED"), "{events:?}");

    let records_dir = launch.records_dir();
    let (code, rest, stderr) = launch.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        rest.is_empty(),
        "nothing follows the readiness line: {rest:?}"
    );
    assert!(records(&records_dir, "a2a_task_received").is_empty());
    let end = records(&records_dir, "task_end");
    assert_eq!(end.len(), 1, "{end:?}");
    assert_eq!(end[0]["task_id"], task_id.as_str());
    assert_eq!(end[0]["exit_status"], "ok");
}

/// S2: the first `ListTasks` after the readiness line already lists the launch task.
#[test]
fn the_first_list_after_the_announcement_lists_the_launch_task() {
    let home = driver_home();
    let (provider, held) = held_provider();
    let project = authenticated_project(&provider.endpoint, "first-list-agent");
    let launch = Launch::start(home.path(), &project.path().join("murmur.yaml"));
    let token = launch.token();

    let tasks = listed(&launch.rpc(&token, "ListTasks", json!({})));
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let state = tasks[0]["status"]["state"].as_str().unwrap();
    assert!(
        matches!(state, "TASK_STATE_SUBMITTED" | "TASK_STATE_WORKING"),
        "{tasks:?}"
    );

    held.release();
    let (code, _, stderr) = launch.finish();
    assert_eq!(code, Some(0), "{stderr}");
}

/// S3: `CancelTask` reaches the launch task while its request is held: the task ends canceled,
/// `task_end` says so, and the session ends.
#[test]
fn cancel_task_ends_the_launch_task_and_its_session() {
    let home = driver_home();
    let (provider, held) = held_provider();
    let project = authenticated_project(&provider.endpoint, "cancel-launch-agent");
    let launch = Launch::start(home.path(), &project.path().join("murmur.yaml"));
    let token = launch.token();
    held.wait_arrived();

    let task_id = listed(&launch.rpc(&token, "ListTasks", json!({})))[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let canceled = launch.rpc(&token, "CancelTask", json!({"id": task_id}));
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );

    let records_dir = launch.records_dir();
    let (code, rest, stderr) = launch.finish();
    drop(held);
    eprintln!("[launch-task] a cancelled launch task's mur run exited {code:?}");
    assert!(rest.is_empty(), "{rest:?}");
    let end = records(&records_dir, "task_end");
    assert_eq!(end.len(), 1, "{end:?} {stderr}");
    assert_eq!(end[0]["task_id"], task_id.as_str());
    assert_eq!(end[0]["exit_status"], "canceled", "{stderr}");
    assert_eq!(code, Some(CANCELED_EXIT), "{stderr}");
}

/// The exit code of a `mur run` whose launch task was cancelled through its door.
const CANCELED_EXIT: i32 = 1;

/// S4: a `single` door running its launch task refuses a second task as busy, and only the
/// launch task ever starts.
#[test]
fn a_single_door_refuses_a_second_task_while_its_launch_task_runs() {
    let home = driver_home();
    let (provider, held) = held_provider();
    let project = agent_project(
        &provider.endpoint,
        "single-launch-agent",
        "",
        &format!(
            "lifecycle:\n  task_acceptance: single\n  after_task: exit\n{AUTHENTICATION_YAML}"
        ),
    );
    let launch = Launch::start(home.path(), &project.path().join("murmur.yaml"));
    let token = launch.token();
    held.wait_arrived();

    let busy = launch.rpc(
        &token,
        "SendMessage",
        returning_immediately(message("m-second", "a second task")),
    );
    let rejected = &busy["result"]["task"];
    assert_eq!(rejected["status"]["state"], "TASK_STATE_REJECTED", "{busy}");
    assert_eq!(
        rejected["status"]["message"]["parts"],
        json!([{"text": "task rejected: capsule is busy", "mediaType": "text/plain"}]),
        "{busy}"
    );

    held.release();
    let records_dir = launch.records_dir();
    let (code, _, stderr) = launch.finish();
    assert_eq!(code, Some(0), "{stderr}");
    let starts = records(&records_dir, "task_start");
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0]["source"], "task_md");
}
