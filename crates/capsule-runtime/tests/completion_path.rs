//! What a delegated child's ending does to the parent that launched it.
//!
//! Every case here runs the real thing: `mur-roost` on a loopback port over a real registry, a
//! real parent capsule as its own `mur run --json` process, and children launched as processes of
//! this one. Nothing is stubbed, and nothing asserts about a queue in isolation — what a test
//! observes is what an operator observes, in the parent's own `trace.jsonl` and in the child's
//! own `completion.json`.
//!
//! One daemon, one registry and one `HOME` are shared by the whole suite, because `HOME` is
//! process-wide and a child resolves its artifacts through it. Each case gets its own parent
//! process and its own workdir.

#[path = "common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use capsule_runtime::delegation::{DelegationStatus, Reporter, SpawnerHandle, SPAWNER_ENV};
use capsule_runtime::{
    launch_child_capsule, ChildLaunchRequest, LaunchedChild, SpawnApproval, Spawner, TrustClass,
};
use common::{component, mur_binary, Roost, ScriptedServer};
use serde_json::Value;
use tempfile::TempDir;

/// The capsule every parent process in this suite runs. Its manifest is the ceiling each child is
/// refereed against, and it accepts a queue of tasks and sleeps between them, which is what lets a
/// completion arrive after the delegation was made.
const PARENT_CAPSULE: &str = "completion-parent";
/// The session the suite registers for itself, to obtain the credential `POST /spawn` wants.
const APPROVER_SESSION: &str = "ses_completion00000000000approver";
const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The variable the `worker` capsule echoes into its result file. Set once for the process, so a
/// child that writes it into `out/result.txt` wrote something no runtime composed.
const MARKER_VAR: &str = "MURMUR_TEST_ALLOWED_VAR";
const MARKER: &str = "child-result-marker-91f3";

struct Suite {
    roost: Roost,
    /// The credential the suite presents to `POST /spawn`. The daemon judges a spawn against the
    /// envelope of the session the credential names, which is a different question from where the
    /// completion is addressed — that is the spawner handle's business.
    credential: String,
    home: TempDir,
    /// Held for the life of the process: every agent capsule here addresses it as its inference
    /// endpoint.
    _inference: ScriptedServer,
}

fn suite() -> &'static Suite {
    static SUITE: OnceLock<Suite> = OnceLock::new();
    SUITE.get_or_init(|| {
        let home = TempDir::new().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var(MARKER_VAR, MARKER);

        let inference = ScriptedServer::always_replying("parent turn done");
        let registry_path = home.path().join(".murmur").join("artifacts");
        std::fs::create_dir_all(&registry_path).unwrap();

        let roost = Roost::start_at(&registry_path, &[PARENT_CAPSULE]);
        common::publish_driver(
            &registry_path,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );

        // The parent's ceiling covers every child below on every axis, and its lifecycle keeps it
        // alive between tasks — a parent that exited after one task would have nowhere for a
        // completion to land.
        common::publish_capsule(
            &registry_path,
            PARENT_CAPSULE,
            "0.1.0",
            &format!(
                "artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    \
                 runtime: driver\ncapabilities:\n  \
                 network:\n    allow: [{endpoint}]\n  \
                 env:\n    allow: [{MARKER_VAR}]\n  \
                 spawn:\n    allow: [worker, waiting-worker, slow-worker]\n\
                 lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n\
                 trace:\n  capture: content\n\
                 inference:\n  transport: http\n  endpoint: {url}\n  model: test-model\n  \
                 api_key: test-key\n  driver:\n    artifact: {DRIVER}\n",
                endpoint = inference.authority(),
                url = inference.endpoint,
            ),
            None,
        );

        // The child of scenario 2, 6b and 7: it echoes the marker into `out/result.txt` and
        // exits, so the result is something only the child could have written.
        common::publish_capsule(
            &registry_path,
            "worker",
            "0.1.0",
            &format!("artifacts: []\ncapabilities:\n  env:\n    allow: [{MARKER_VAR}]\n"),
            Some(&component("capsule-env-echo.wasm")),
        );

        // A child that does not finish until its parent puts `targets.txt` in its directory,
        // which is what makes "kill the parent while the child is still working" a thing a test
        // can arrange rather than race.
        common::publish_capsule(
            &registry_path,
            "waiting-worker",
            "0.1.0",
            "artifacts: []\n",
            Some(&component("capsule-path-reader.wasm")),
        );

        // An agent child: it binds a port and waits, so it is still running when the launch
        // returns and can be killed. `mur_version` is deliberately stale, so `mur run` writes one
        // predictable line to its stderr before anything else — the line a crash quotes back.
        common::publish_capsule(
            &registry_path,
            "slow-worker",
            "0.1.0",
            &format!(
                "mur_version: \"0.0.1\"\nartifacts:\n  - name: {DRIVER}\n    version: \
                 {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    \
                 allow: [{endpoint}]\nlifecycle:\n  task_acceptance: queue\n  after_task: sleep\n\
                 inference:\n  transport: http\n  endpoint: {url}\n  model: test-model\n  \
                 api_key: test-key\n  driver:\n    artifact: {DRIVER}\n",
                endpoint = inference.authority(),
                url = inference.endpoint,
            ),
            None,
        );

        let credential = roost.register(APPROVER_SESSION, PARENT_CAPSULE, "0.1.0");
        std::env::set_var(capsule_runtime::MUR_BINARY_ENV, mur_binary());

        Suite {
            roost,
            credential,
            home,
            _inference: inference,
        }
    })
}

// ── A real parent capsule, as its own process ─────────────────────────────────

/// One `mur run --json` process running [`PARENT_CAPSULE`], with the address and session id a
/// completion has to name to reach it.
struct RunningParent {
    process: Option<Child>,
    workdir: TempDir,
    session_id: String,
    url: String,
}

impl RunningParent {
    fn start() -> Self {
        let suite = suite();
        let workdir = TempDir::new().unwrap();
        let mut process = Command::new(mur_binary())
            .args([
                "run",
                "--capsule",
                PARENT_CAPSULE,
                "--capsule-version",
                "0.1.0",
                "--workdir",
                workdir.path().to_str().unwrap(),
                "--json",
                "--no-env-file",
            ])
            .current_dir(workdir.path())
            .env_clear()
            .env("HOME", suite.home.path())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("MURMUR_ROOST_URL", &suite.roost.url)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the parent capsule process starts");

        let stdout = process.stdout.take().expect("the parent reports on stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let first = match reader.read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line.trim_end().to_string()),
            };
            let _ = tx.send(first);
            // Keep the pipe drained, or a parent that logs after its launch line blocks.
            let mut rest = String::new();
            while matches!(reader.read_line(&mut rest), Ok(read) if read > 0) {
                rest.clear();
            }
        });
        let line = rx
            .recv_timeout(Duration::from_secs(180))
            .ok()
            .flatten()
            .expect("the parent prints its --json launch line");
        let report: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("the parent's launch line did not parse: {error}: {line}")
        });
        let session_id = report["session_id"]
            .as_str()
            .expect("the launch line carries a session id")
            .to_string();
        let url = report["url"]
            .as_str()
            .filter(|url| !url.is_empty())
            .map(|url| format!("http://{url}"))
            .expect("the launch line carries an address");

        Self {
            process: Some(process),
            workdir,
            session_id,
            url,
        }
    }

    /// Where the parent wants completions of its delegations sent.
    fn spawner(&self, trust: TrustClass) -> Spawner {
        Spawner {
            url: self.url.clone(),
            session_id: self.session_id.clone(),
            context_id: format!("ctx_{}", uuid_hex()),
            trust,
        }
    }

    fn trace_path(&self) -> PathBuf {
        self.workdir
            .path()
            .join(".murmur")
            .join(&self.session_id)
            .join("trace.jsonl")
    }

    /// Every event the parent has written so far.
    fn trace(&self) -> Vec<Value> {
        std::fs::read_to_string(self.trace_path())
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The parent's `task_start` for `delegation_id`, once it has been written.
    fn wait_for_completion_task(&self, delegation_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(event) = self.trace().into_iter().find(|event| {
                event["event_type"] == "task_start" && event["delegation_id"] == delegation_id
            }) {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "the parent never started a task for delegation {delegation_id}; trace:\n{}",
                std::fs::read_to_string(self.trace_path()).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Blocks until the parent has finished `task_id`, so what it sent the model is on disk.
    fn wait_for_task_end(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if self
                .trace()
                .iter()
                .any(|event| event["event_type"] == "task_end" && event["task_id"] == task_id)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the parent never finished task {task_id}; trace:\n{}",
                std::fs::read_to_string(self.trace_path()).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

impl Drop for RunningParent {
    fn drop(&mut self) {
        self.kill();
    }
}

fn uuid_hex() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

// ── Launching children ────────────────────────────────────────────────────────

/// One agent child stages at a time.
///
/// A spawn approval is valid for 60 seconds from the `POST /spawn` that granted it, and an agent
/// child spends that window staging a driver and binding a port. Two of them doing it at once on a
/// loaded machine can outlast the approval they were launched under, and the child is then refused
/// at registration rather than run unrefereed. Script children are cheap and take no slot.
fn agent_child_slot() -> std::sync::MutexGuard<'static, ()> {
    static SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SLOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `POST /spawn`, then a launch — the part the model-facing delegate tool will play later.
fn launch(
    parent_dir: &Path,
    name: &str,
    env_allow: &[&str],
    spawner: Option<Spawner>,
) -> LaunchedChild {
    let suite = suite();
    let answer = suite.roost.permission(&suite.credential, name, "0.1.0");
    let approval = answer["approval"]
        .as_str()
        .unwrap_or_else(|| panic!("spawn was refused: {answer}"))
        .to_string();

    launch_child_capsule(ChildLaunchRequest {
        parent_accessible_workdir: parent_dir.to_path_buf(),
        capsule_name: name.to_string(),
        capsule_version: "0.1.0".to_string(),
        grant: SpawnApproval::new(approval),
        child_env_allow: env_allow.iter().map(|name| name.to_string()).collect(),
        roost_url: suite.roost.url.clone(),
        spawner,
    })
    .unwrap_or_else(|error| panic!("launching '{name}' failed: {error}"))
}

/// The child's own record of how it ended, once the delivery it describes has settled.
///
/// A reporter writes the file before it posts and rewrites it with the result, so the record is
/// briefly readable in a state that says neither that the completion arrived nor why it did not.
/// Waiting for the settled one is what stops a case reading that intermediate write.
fn wait_for_completion(child: &LaunchedChild) -> Value {
    let path = child.workdir.join("completion.json");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                if is_settled(&parsed) {
                    return parsed;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no settled {} appeared for the child at {}; what is there: {}",
            "completion.json",
            child.workdir.display(),
            std::fs::read_to_string(&path).unwrap_or_else(|_| "nothing".to_string())
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a completion record says what became of the delivery: it arrived, it was refused for a
/// named reason, or it was a `terminated` delegation, which is posted to nobody.
fn is_settled(completion: &Value) -> bool {
    completion["delivered"] == Value::Bool(true)
        || !completion["delivery_error"].is_null()
        || completion["status"] == DelegationStatus::Terminated.as_str()
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn dirs_under(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

fn wait_for_new_dir(parent: &Path, before: &[PathBuf]) -> Option<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(fresh) = dirs_under(parent)
            .into_iter()
            .find(|dir| !before.contains(dir))
        {
            return Some(fresh);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

// ── 1. A child knows its spawner, from an injected value ──────────────────────

/// The handle a child holds is the one its parent composed for this launch. A child nobody
/// delegated holds none, and a child that allowlists the name still gets the injected one: the
/// runtime-owned value is applied last.
#[test]
fn a_child_knows_its_spawner_from_an_injected_value_and_not_an_inherited_one() {
    let parent_dir = TempDir::new().unwrap();
    // The decoy the launching process itself holds. Nothing may reach a child from here.
    std::env::set_var(SPAWNER_ENV, "decoy-not-a-handle");

    let parent = RunningParent::start();
    let spawner = parent.spawner(TrustClass::Trusted);

    let delegated = launch(
        parent_dir.path(),
        "worker",
        &[MARKER_VAR],
        Some(spawner.clone()),
    );
    let undelegated = launch(parent_dir.path(), "worker", &[MARKER_VAR], None);
    let allowlisting = launch(
        parent_dir.path(),
        "worker",
        &[MARKER_VAR, SPAWNER_ENV],
        Some(spawner.clone()),
    );

    fn injected(child: &LaunchedChild) -> Vec<&String> {
        child
            .env
            .iter()
            .filter(|(key, _)| key == SPAWNER_ENV)
            .map(|(_, value)| value)
            .collect()
    }

    let values = injected(&delegated);
    assert_eq!(values.len(), 1, "{:?}", delegated.env);
    let handle = SpawnerHandle::parse(values[0]).expect("the injected value parses as a handle");
    assert_eq!(handle.url, spawner.url);
    assert_eq!(handle.session_id, parent.session_id);
    assert_eq!(handle.context_id, spawner.context_id);
    assert_eq!(handle.trust, TrustClass::Trusted);
    assert_eq!(
        Some(handle.delegation_id.clone()),
        delegated.delegation_id,
        "the launcher's delegation id is what was injected"
    );
    assert!(
        handle.delegation_id.starts_with("dlg_"),
        "{}",
        handle.delegation_id
    );

    assert!(
        injected(&undelegated).is_empty(),
        "a child nobody delegated holds no handle: {:?}",
        undelegated.env
    );
    assert_eq!(undelegated.delegation_id, None);

    let allowlisted = injected(&allowlisting);
    assert_eq!(allowlisted.len(), 1, "{:?}", allowlisting.env);
    assert_ne!(allowlisted[0], "decoy-not-a-handle");
    assert_eq!(
        SpawnerHandle::parse(allowlisted[0])
            .expect("the injected value parses")
            .session_id,
        parent.session_id
    );

    // Two launches from one spawner report under two ids, never one.
    assert_ne!(delegated.delegation_id, allowlisting.delegation_id);
    std::env::remove_var(SPAWNER_ENV);
}

// ── 2. A finished child's completion arrives at a real parent ─────────────────

/// The child writes its outcome, posts it, and the parent files it as a `completion`-origin task
/// in the background lane — joined to the delegation by the id all three carry. The result stays
/// in the child's own directory: it is in no line of the parent's trace and in no part of the
/// completion's text.
#[test]
fn a_finished_childs_completion_arrives_at_its_parent_as_a_completion_task() {
    let parent = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();

    let child = launch(
        parent_dir.path(),
        "worker",
        &[MARKER_VAR],
        Some(parent.spawner(TrustClass::Trusted)),
    );
    let delegation_id = child
        .delegation_id
        .clone()
        .expect("a delegated launch has an id");

    let completion = wait_for_completion(&child);
    assert_eq!(completion["status"], DelegationStatus::Ok.as_str());
    assert_eq!(completion["reported_by"], Reporter::Child.as_str());
    assert_eq!(completion["delivered"], true, "{completion}");
    assert_eq!(completion["result_path"], "out/result.txt");
    assert_eq!(completion["delegation_id"], delegation_id);
    assert_eq!(completion["capsule_name"], "worker");
    assert_eq!(completion["session_id"], child.session_id);
    assert_eq!(completion["workdir"], child.workdir.display().to_string());

    // The three-way equality that joins a completion to the delegation that produced it.
    let task_start = parent.wait_for_completion_task(&delegation_id);
    assert_eq!(task_start["origin"], "completion");
    assert_eq!(task_start["trust"], "trusted");
    assert_eq!(task_start["lane"], "bg");
    assert_eq!(task_start["delegation_id"], delegation_id);
    assert_eq!(task_start["source"], "a2a");

    // The result is where the completion said it was, and nowhere else.
    let result = std::fs::read_to_string(child.workdir.join("out").join("result.txt"))
        .expect("the child wrote the file the completion named");
    assert!(result.contains(MARKER), "{result}");

    let trace = std::fs::read_to_string(parent.trace_path()).unwrap_or_default();
    assert!(
        !trace.contains(MARKER),
        "the child's own output must not reach the parent's trace:\n{trace}"
    );
    assert!(
        task_start["message_parts_bytes"].as_u64().unwrap_or(0) > 0,
        "a completion task carries a message"
    );
    parent.wait_for_task_end(task_start["task_id"].as_str().unwrap());
    // The parent captures what it sent the model, so the completion's own text is on disk: it
    // names the path and carries nothing the child wrote.
    assert!(
        parent_sent(&parent, "out/result.txt"),
        "the completion's message names where the result is"
    );
    assert!(
        !parent_sent(&parent, MARKER),
        "the completion's message must name the result, never carry it"
    );

    // What an operator reads: the task row names the lane the completion waited in and the
    // delegation it reports on.
    let steps = Command::new(mur_binary())
        .args(["trace", "steps", parent.trace_path().to_str().unwrap()])
        .output()
        .expect("mur trace steps runs");
    let rendered = String::from_utf8_lossy(&steps.stdout).to_string();
    let row = rendered
        .lines()
        .find(|line| line.contains("completion/trusted"))
        .unwrap_or_else(|| panic!("no completion task row in:\n{rendered}"));
    assert!(row.contains("lane bg"), "{row}");
    assert!(
        row.contains(&format!("delegation {delegation_id}")),
        "{row}"
    );
}

/// Whether `needle` appears anywhere in what the parent recorded of this session — its trace and,
/// because the parent capsule declares `trace.capture: content`, the driver request bodies behind
/// it. That is where a completion's message text is observable after the task has run.
fn parent_sent(parent: &RunningParent, needle: &str) -> bool {
    common::find_in_files(
        &parent
            .workdir
            .path()
            .join(".murmur")
            .join(&parent.session_id),
        needle,
    )
    .is_some()
}

// ── 5. A child that crashes without reporting ─────────────────────────────────

/// The launcher reports for a child that could not: the delegation is recorded as `crashed`, with
/// the exit status and the child's own stderr, and the parent is told.
#[test]
fn a_child_killed_without_reporting_is_reported_by_its_launcher() {
    let parent = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();

    let child = {
        let _slot = agent_child_slot();
        launch(
            parent_dir.path(),
            "slow-worker",
            &[],
            Some(parent.spawner(TrustClass::Trusted)),
        )
    };
    let delegation_id = child
        .delegation_id
        .clone()
        .expect("a delegated launch has an id");
    let pid = child.pid();
    assert!(pid > 0);
    // The stale `mur_version` warning is the child's own line, and it is on its stderr before
    // anything else the child does.
    wait_for(
        "the child to write its stderr line",
        Duration::from_secs(60),
        || {
            child
                .stderr_tail()
                .iter()
                .any(|line| line.contains("manifest requires mur 0.0.1"))
        },
    );

    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "kill -9 {pid} failed");

    let completion = wait_for_completion(&child);
    assert_eq!(completion["status"], DelegationStatus::Crashed.as_str());
    assert_eq!(completion["reported_by"], Reporter::Launcher.as_str());
    assert_eq!(completion["delivered"], true, "{completion}");
    let detail = completion["detail"]
        .as_str()
        .expect("a crash carries detail");
    assert!(detail.contains("signal: 9"), "{detail}");
    assert!(detail.contains("manifest requires mur 0.0.1"), "{detail}");

    let task_start = parent.wait_for_completion_task(&delegation_id);
    assert_eq!(task_start["origin"], "completion");
    assert_eq!(task_start["lane"], "bg");
    assert_eq!(task_start["delegation_id"], delegation_id);
    parent.wait_for_task_end(task_start["task_id"].as_str().unwrap());
    assert!(
        parent_sent(&parent, "status: crashed"),
        "the completion's message says what happened"
    );

    // The handle is still alive, and ending it now is not an error.
    drop(child);
}

// ── 6a. A completion whose parent is gone ─────────────────────────────────────

/// The parent's process is killed while its child is still working. The child finishes, cannot
/// deliver, and records that — the delivery failure is not the child's session failing.
#[test]
fn a_completion_with_no_parent_left_is_recorded_rather_than_dropped() {
    let mut parent = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();
    let spawner = parent.spawner(TrustClass::Trusted);
    let children_dir = parent_dir.path().join(".murmur").join("children");
    let before = dirs_under(&children_dir);

    // The child waits for `targets.txt` before it finishes, so the parent can be killed while it
    // is demonstrably still working rather than in a race with its exit.
    let child = std::thread::scope(|scope| {
        let launch_thread =
            scope.spawn(|| launch(parent_dir.path(), "waiting-worker", &[], Some(spawner)));
        let child_dir = wait_for_new_dir(&children_dir, &before).expect("the child's directory");
        parent.kill();
        std::fs::write(child_dir.join("targets.part"), "targets.txt").unwrap();
        std::fs::rename(
            child_dir.join("targets.part"),
            child_dir.join("targets.txt"),
        )
        .unwrap();
        launch_thread.join().unwrap()
    });

    let completion = wait_for_completion(&child);
    assert_eq!(
        completion["status"],
        DelegationStatus::Ok.as_str(),
        "the child's own session succeeded; only the delivery failed: {completion}"
    );
    assert_eq!(completion["reported_by"], Reporter::Child.as_str());
    assert_eq!(completion["delivered"], false, "{completion}");
    let reason = completion["delivery_error"]
        .as_str()
        .expect("the refusal is recorded");
    assert!(reason.contains("failed to connect"), "{reason}");

    wait_for(
        "the child to say on its stderr that the completion went nowhere",
        Duration::from_secs(60),
        || {
            child
                .stderr_tail()
                .iter()
                .any(|line| line.contains("could not be delivered"))
        },
    );
}

// ── 6b. A completion addressed to a session that is not there ─────────────────

/// One capsule's address, another capsule's session id — the shape a parent that restarted onto
/// the same port leaves behind. The door refuses, the addressed capsule files no completion task,
/// and the child records the refusal.
#[test]
fn a_completion_addressed_to_another_session_is_refused_and_recorded() {
    let addressed = RunningParent::start();
    let elsewhere = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();
    let before = addressed
        .trace()
        .iter()
        .filter(|event| event["origin"] == "completion")
        .count();

    let misaddressed = Spawner {
        url: addressed.url.clone(),
        session_id: elsewhere.session_id.clone(),
        context_id: format!("ctx_{}", uuid_hex()),
        trust: TrustClass::Trusted,
    };
    let child = launch(
        parent_dir.path(),
        "worker",
        &[MARKER_VAR],
        Some(misaddressed),
    );

    let completion = wait_for_completion(&child);
    assert_eq!(completion["status"], DelegationStatus::Ok.as_str());
    assert_eq!(completion["delivered"], false, "{completion}");
    let reason = completion["delivery_error"]
        .as_str()
        .expect("the refusal is recorded");
    assert!(
        reason.contains(&elsewhere.session_id),
        "the refusal names the addressed session: {reason}"
    );
    assert!(
        !reason.contains(&addressed.session_id),
        "the refusal must not name the session actually running here: {reason}"
    );

    // Nothing reached the addressed capsule's queue. Give it a moment to have done so wrongly.
    std::thread::sleep(Duration::from_secs(2));
    let after = addressed
        .trace()
        .iter()
        .filter(|event| event["origin"] == "completion")
        .count();
    assert_eq!(
        after,
        before,
        "the addressed capsule must file no completion task; trace:\n{}",
        std::fs::read_to_string(addressed.trace_path()).unwrap_or_default()
    );
}

// ── 7. Trust inherits from the delegating task ────────────────────────────────

/// Two delegations of one worker from one parent, one untrusted and one trusted, produce two
/// completion tasks whose classes are the classes the delegations carried. Nothing decides trust
/// a second time.
#[test]
fn a_completions_trust_is_the_delegating_tasks_trust() {
    let parent = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();

    let mut observed = Vec::new();
    for trust in [TrustClass::Untrusted, TrustClass::Trusted] {
        let child = launch(
            parent_dir.path(),
            "worker",
            &[MARKER_VAR],
            Some(parent.spawner(trust)),
        );
        let delegation_id = child.delegation_id.clone().expect("a delegated launch");
        let completion = wait_for_completion(&child);
        assert_eq!(completion["delivered"], true, "{completion}");
        let task_start = parent.wait_for_completion_task(&delegation_id);
        parent.wait_for_task_end(task_start["task_id"].as_str().unwrap());
        observed.push((
            task_start["origin"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            task_start["trust"].as_str().unwrap_or_default().to_string(),
        ));
    }

    assert_eq!(
        observed,
        vec![
            ("completion".to_string(), "untrusted".to_string()),
            ("completion".to_string(), "trusted".to_string()),
        ]
    );

    // The untrusted one's payload is fenced by the same rule every untrusted task's is, and by
    // the same marker: the parent's captured wire body carries it.
    assert!(
        parent_sent(&parent, "<untrusted-content source=task:completion>"),
        "an untrusted completion must be fenced like every other untrusted task"
    );
}

// ── The launcher reports once, and only for a child that did not ──────────────

/// A delegation the parent ends itself is recorded as `terminated` and posted to nobody: the only
/// party that would be told is the party that did it.
#[test]
fn a_delegation_the_parent_ends_is_recorded_and_not_announced() {
    let parent = RunningParent::start();
    let parent_dir = TempDir::new().unwrap();

    let mut child = {
        let _slot = agent_child_slot();
        launch(
            parent_dir.path(),
            "slow-worker",
            &[],
            Some(parent.spawner(TrustClass::Trusted)),
        )
    };
    let delegation_id = child.delegation_id.clone().expect("a delegated launch");
    let workdir = child.workdir.clone();
    child.shutdown().expect("the parent ends the delegation");

    let deadline = Instant::now() + Duration::from_secs(60);
    let completion = loop {
        if let Ok(raw) = std::fs::read_to_string(workdir.join("completion.json")) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                break parsed;
            }
        }
        assert!(Instant::now() < deadline, "no completion.json was recorded");
        std::thread::sleep(Duration::from_millis(100));
    };

    assert_eq!(completion["status"], DelegationStatus::Terminated.as_str());
    assert_eq!(completion["reported_by"], Reporter::Launcher.as_str());
    assert_eq!(completion["delivered"], false, "{completion}");
    assert!(completion["delivery_error"].is_null(), "{completion}");

    // Nothing was posted, so the parent has no task for it.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !parent
            .trace()
            .iter()
            .any(|event| event["delegation_id"] == delegation_id.as_str()),
        "a terminated delegation is announced to nobody"
    );
}
