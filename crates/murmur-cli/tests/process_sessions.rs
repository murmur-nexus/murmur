//! What a `transport: process` capsule remembers between messages: the map from an A2A context to
//! the harness session that holds its conversation, and what happens when the harness cannot find
//! a session it is handed.
//!
//! Every test publishes the fixture process driver, puts the fixture fake harness on `PATH` as
//! `fixture-cli` under a profile that keeps a conversation of its own, and reads back
//! `out/result.txt`, `trace.jsonl` and `~/.murmur/conversations/`. The fake harness is a bash
//! script, so these tests are Unix-only — which every platform murmur targets is.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const DRIVER: &str = "fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";
const CAPSULE: &str = "process-sessions";
const HARNESS: &str = "fixture-harness";
const SECRET_TASK: &str = "the secret is PLUM-42";
const RECALL_TASK: &str = "what is the secret";
const SECRET: &str = "PLUM-42";

// ── the capsule ───────────────────────────────────────────────────────────────

/// How a capsule's own conversation memory is declared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Conversation {
    Stateless,
    Threaded,
}

/// A published driver, a project, a scratch `HOME`, and the fake harness on a `PATH` of its own.
struct Capsule {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    bin_dir: PathBuf,
    manifest: PathBuf,
}

struct Built {
    conversation: Conversation,
    queue: bool,
    record: bool,
    hooks: String,
}

impl Built {
    fn new() -> Self {
        Self {
            conversation: Conversation::Threaded,
            queue: false,
            record: true,
            hooks: String::new(),
        }
    }

    fn stateless(mut self) -> Self {
        self.conversation = Conversation::Stateless;
        self
    }

    /// `task_acceptance: queue` + `after_task: sleep`: the capsule stays up and takes A2A
    /// messages, so several contexts can be driven through one life.
    fn queueing(mut self) -> Self {
        self.queue = true;
        self
    }

    fn record_off(mut self) -> Self {
        self.record = false;
        self
    }

    fn hooks(mut self, yaml: &str) -> Self {
        self.hooks = yaml.to_string();
        self
    }

    fn build(self) -> Capsule {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();

        let driver = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("process-driver/tool/process-driver.wasm"),
            "",
        );
        common::publish_local(&home, &driver).success();

        // Only this capsule's `PATH` points here, so `fixture-cli` is the fake harness and
        // nothing else on the host is.
        let bin_dir = project.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("fixture-cli");
        fs::copy(common::fixture_path("process-driver/fake-harness"), &script).unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&script, perms).unwrap();

        let acceptance = if self.queue {
            "  task_acceptance: queue\n  after_task: sleep\n"
        } else {
            "  task_acceptance: single\n  after_task: exit\n"
        };
        let conversation = match self.conversation {
            Conversation::Stateless => "  conversation: stateless\n",
            Conversation::Threaded => "  conversation: threaded\n",
        };
        let context = if self.record {
            String::new()
        } else {
            "context:\n  record: off\n".to_string()
        };

        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: {CAPSULE}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n{hooks}\
                 lifecycle:\n{acceptance}{conversation}{context}\
                 capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n",
                hooks = self.hooks,
            ),
        )
        .unwrap();

        Capsule {
            home,
            project,
            _artifacts: artifacts,
            bin_dir,
            manifest,
        }
    }
}

/// One `mur run` that ran a task and exited.
struct Run {
    output: Output,
    text: String,
    workdir: PathBuf,
}

impl Capsule {
    /// The bin directory first, so `fixture-cli` is the fake harness, then the host's own `PATH`
    /// so the harness's `mkdir` and `tr` resolve.
    fn path_value(&self) -> String {
        format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
        )
    }

    fn mur(&self, profile: &str) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", self.path_value())
            .env("FIXTURE_HARNESS_PROFILE", profile)
            .current_dir(self.project.path());
        command
    }

    /// One whole capsule life: a single task, then exit.
    fn run(&self, profile: &str, args: &[&str]) -> Run {
        let mut command = self.mur(profile);
        command.args([
            "run",
            "--manifest",
            self.manifest.to_str().unwrap(),
            "--verbose",
        ]);
        command.args(args);
        let output = command.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let workdir = if text.contains("workdir: ") {
            common::parse_workdir_from_stdout(&text)
        } else {
            self.project.path().to_path_buf()
        };
        Run {
            output,
            text,
            workdir,
        }
    }

    fn task(&self, profile: &str, context: &str, task: &str) -> Run {
        self.run(profile, &["--context", context, "--task", task])
    }

    /// Rewrite the manifest to `lifecycle.conversation: stateless`, so a later `--resume` is the
    /// only thing asking for the conversation to continue.
    fn make_stateless(&self) {
        let text = fs::read_to_string(&self.manifest).unwrap();
        assert!(text.contains("  conversation: threaded\n"), "{text}");
        fs::write(
            &self.manifest,
            text.replace("  conversation: threaded\n", "  conversation: stateless\n"),
        )
        .unwrap();
    }

    /// The map file one context wrote, whether or not it exists.
    fn map_file(&self, context: &str) -> PathBuf {
        self.home
            .path()
            .join(".murmur/conversations")
            .join(CAPSULE)
            .join(context)
            .join("harness-session.json")
    }

    fn map_entry(&self, context: &str) -> Value {
        let path = self.map_file(context);
        let raw = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("{} is not JSON ({err})", raw))
    }

    fn stored_session(&self, context: &str) -> String {
        self.map_entry(context)["session_id"]
            .as_str()
            .expect("the entry names a session")
            .to_string()
    }

    /// The harness's own store — not murmur's map.
    fn harness_store(&self) -> PathBuf {
        self.home.path().join("fake-harness-sessions")
    }

    fn harness_sessions(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.harness_store())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn conversations_root(&self) -> PathBuf {
        self.home.path().join(".murmur/conversations")
    }
}

impl Run {
    fn succeeded(self) -> Self {
        assert!(
            self.output.status.success(),
            "mur run failed:\n{}",
            self.text
        );
        self
    }

    fn failed(self) -> Self {
        assert!(
            !self.output.status.success(),
            "mur run succeeded:\n{}",
            self.text
        );
        self
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir.join("out").join("result.txt")).unwrap_or_default()
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.workdir.join("trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// The one `harness_start` this run wrote.
    fn launch(&self) -> Value {
        let mut found = self.of_type("harness_start");
        assert_eq!(
            found.len(),
            1,
            "expected one harness_start, got {found:#?}\n{}",
            self.text
        );
        found.remove(0)
    }
}

// ── a capsule that stays up ───────────────────────────────────────────────────

/// A `queue` + `sleep` capsule launched as its own `mur run` process, and the door it printed.
struct LiveCapsule {
    capsule: Capsule,
    child: std::process::Child,
    url: String,
    workdir: PathBuf,
}

impl Drop for LiveCapsule {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LiveCapsule {
    fn start(capsule: Capsule, profile: &str) -> Self {
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .args(["run", "--manifest"])
            .arg(&capsule.manifest)
            .arg("--json")
            .current_dir(capsule.project.path())
            .env_clear()
            .env("HOME", capsule.home.path())
            .env("PATH", capsule.path_value())
            .env("FIXTURE_HARNESS_PROFILE", profile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("mur run should start");

        let (tx, rx) = mpsc::channel::<Value>();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.get("url").is_some() {
                        let _ = tx.send(value);
                    }
                }
            }
        });
        let stderr = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[capsule] {line}");
            }
        });

        let startup = match rx.recv_timeout(Duration::from_secs(180)) {
            Ok(startup) => startup,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for the capsule to print where its door is");
            }
        };
        Self {
            capsule,
            child,
            url: startup["url"].as_str().unwrap().to_string(),
            workdir: PathBuf::from(startup["workdir"].as_str().unwrap()),
        }
    }

    /// One A2A `message/send` under `context`, waited out to completion.
    fn send(&self, message_id: &str, context: &str, text: &str, task_number: usize) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": message_id,
                    "contextId": context,
                    "role": "user",
                    "parts": [{"text": text}]
                }
            }
        })
        .to_string();
        let response = post(&self.url, &body);
        assert_eq!(
            response["result"]["status"]["state"], "submitted",
            "the task should be accepted; got: {response}"
        );
        assert!(response["result"]["id"].is_string(), "{response}");
        self.wait_for_task_ends(task_number);
    }

    /// What the last finished task answered.
    fn result(&self) -> String {
        fs::read_to_string(self.workdir.join("out").join("result.txt")).unwrap_or_default()
    }

    fn wait_for_task_ends(&self, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let ends = self
                .events()
                .iter()
                .filter(|event| event["event_type"] == "task_end")
                .count();
            if ends >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} task(s) to finish"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.workdir.join("trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn launches(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| event["event_type"] == "harness_start")
            .collect()
    }
}

fn post(addr: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(addr).expect("should connect to the capsule");
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: \
         {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    let mut response = String::new();
    reader.read_to_string(&mut response).ok();
    serde_json::from_str(&response).unwrap_or_else(|_| serde_json::json!({"_raw": response}))
}

fn assert_mode(path: &Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .unwrap_or_else(|err| panic!("{} must exist: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode,
        expected,
        "{} must be {expected:o}, got {mode:04o}",
        path.display()
    );
}

/// Every `conversation.jsonl` anywhere under `root`.
fn record_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(record_files(&path));
        } else if path.file_name().is_some_and(|n| n == "conversation.jsonl") {
            found.push(path);
        }
    }
    found
}

// ── S1: one context, two messages, one conversation ───────────────────────────

/// The whole point: the second message is answered from what the first one said.
#[test]
fn a_second_message_in_one_context_resumes_the_harness_session() {
    let live = LiveCapsule::start(Built::new().queueing().build(), "memory");

    live.send("m1", "ctx_thread", SECRET_TASK, 1);
    live.send("m2", "ctx_thread", RECALL_TASK, 2);

    let answer = live.result();
    assert!(
        answer.contains(SECRET),
        "the harness should answer from the first message; got: {answer}"
    );

    let launches = live.launches();
    assert_eq!(launches.len(), 2, "two turns, two launches: {launches:#?}");
    assert_eq!(launches[0]["session_mode"], "new");
    assert_eq!(launches[1]["session_mode"], "resume");
    assert_eq!(
        launches[0]["harness_session_id"], launches[1]["harness_session_id"],
        "the second turn continues the session the first established"
    );

    assert_eq!(
        live.capsule.harness_sessions().len(),
        1,
        "the harness kept one conversation, not one per message"
    );
}

// ── S2: two contexts, two conversations ───────────────────────────────────────

#[test]
fn two_contexts_get_two_sessions_and_two_map_files() {
    let live = LiveCapsule::start(Built::new().queueing().build(), "memory");

    live.send("m1", "ctx_one", SECRET_TASK, 1);
    live.send("m2", "ctx_two", SECRET_TASK, 2);

    let launches = live.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[0]["session_mode"], "new");
    assert_eq!(launches[1]["session_mode"], "new");
    assert_ne!(
        launches[0]["harness_session_id"], launches[1]["harness_session_id"],
        "a context that has said nothing before starts its own conversation"
    );

    let one = live.capsule.stored_session("ctx_one");
    let two = live.capsule.stored_session("ctx_two");
    assert_ne!(one, two);
    assert_eq!(live.capsule.harness_sessions().len(), 2);
}

// ── S3: the map outlives the capsule ──────────────────────────────────────────

#[test]
fn a_restarted_capsule_continues_the_same_context() {
    let capsule = Built::new().build();

    let first = capsule
        .task("memory", "ctx_restart", SECRET_TASK)
        .succeeded();
    assert_eq!(first.launch()["session_mode"], "new");
    let established = first.launch()["harness_session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let path = capsule.map_file("ctx_restart");
    let entry = capsule.map_entry("ctx_restart");
    assert_eq!(entry["type"], "murmur.harness-session");
    assert_eq!(entry["session_id"], established.as_str());
    assert_eq!(entry["harness"], HARNESS);
    assert_eq!(entry["driver"], DRIVER);
    assert!(entry["created_ms"].as_u64().unwrap() > 0);

    assert_mode(&path, 0o600);
    let mut dir = path.parent();
    for _ in 0..3 {
        let current = dir.expect("three directories above the map file");
        assert_mode(current, 0o700);
        dir = current.parent();
    }

    // A whole second capsule life, sharing only the home and the context id.
    let second = capsule
        .task("memory", "ctx_restart", RECALL_TASK)
        .succeeded();
    assert!(
        second.result().contains(SECRET),
        "the harness should answer from the first run; got: {}",
        second.result()
    );
    assert_eq!(second.launch()["session_mode"], "resume");
    assert_eq!(second.launch()["harness_session_id"], established.as_str());

    assert!(
        record_files(&capsule.conversations_root()).is_empty(),
        "the harness owns the conversation; the runtime writes no conversation.jsonl"
    );
}

// ── S4: stateless starts from nothing every time ──────────────────────────────

#[test]
fn stateless_starts_a_new_conversation_every_task() {
    let capsule = Built::new().stateless().build();

    let first = capsule.task("memory", "ctx_flat", SECRET_TASK).succeeded();
    let second = capsule.task("memory", "ctx_flat", RECALL_TASK).succeeded();

    assert_eq!(first.launch()["session_mode"], "new");
    assert_eq!(second.launch()["session_mode"], "new");
    assert_ne!(
        first.launch()["harness_session_id"],
        second.launch()["harness_session_id"]
    );
    assert!(
        !second.result().contains(SECRET),
        "a stateless task starts from nothing; got: {}",
        second.result()
    );
    assert!(
        !capsule.map_file("ctx_flat").exists(),
        "a capsule that remembers nothing writes nothing"
    );
}

// ── S5: a resume the harness cannot find ──────────────────────────────────────

#[test]
fn a_session_the_harness_lost_fails_the_turn_and_keeps_failing() {
    let capsule = Built::new().build();
    capsule.task("memory", "ctx_lost", SECRET_TASK).succeeded();
    let established = capsule.stored_session("ctx_lost");

    // The harness's own store, not murmur's map: the conversation is gone, the id is not.
    let store = capsule.harness_store();
    for entry in fs::read_dir(&store).unwrap().filter_map(Result::ok) {
        fs::remove_file(entry.path()).unwrap();
    }
    fs::remove_dir(&store).unwrap();

    let failed = capsule.task("memory", "ctx_lost", RECALL_TASK).failed();
    let map_path = capsule.map_file("ctx_lost").display().to_string();
    for expected in [
        "error[E-RUN-036]",
        "ctx_lost",
        established.as_str(),
        HARNESS,
        "no conversation found with session id",
        map_path.as_str(),
    ] {
        assert!(
            failed.text.contains(expected),
            "the failure must name {expected}:\n{}",
            failed.text
        );
    }

    assert_eq!(
        capsule.stored_session("ctx_lost"),
        established,
        "the id is left exactly as it was"
    );
    assert!(
        !store.exists(),
        "a failed resume must not quietly start a new conversation"
    );

    // And it keeps failing, rather than healing itself on the next try.
    let again = capsule.task("memory", "ctx_lost", RECALL_TASK).failed();
    assert!(again.text.contains("error[E-RUN-036]"), "{}", again.text);
    assert_eq!(capsule.stored_session("ctx_lost"), established);
}

// ── S6: the harness names its own session ─────────────────────────────────────

#[test]
fn the_id_the_harness_reports_is_the_one_that_is_remembered() {
    let capsule = Built::new().build();

    let first = capsule
        .task("memory-renames", "ctx_renamed", SECRET_TASK)
        .succeeded();
    let asked_for = first.launch()["harness_session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let stored = capsule.stored_session("ctx_renamed");
    assert_eq!(
        stored,
        format!("renamed-{asked_for}"),
        "what the harness reported wins over what the runtime asked for"
    );

    let second = capsule
        .task("memory-renames", "ctx_renamed", RECALL_TASK)
        .succeeded();
    assert_eq!(second.launch()["session_mode"], "resume");
    assert_eq!(second.launch()["harness_session_id"], stored.as_str());
    assert!(second.result().contains(SECRET), "{}", second.result());
}

// ── S7: mur run --resume ──────────────────────────────────────────────────────

#[test]
fn resume_continues_the_harness_session_and_compact_is_refused() {
    let capsule = Built::new().build();
    capsule
        .task("memory", "ctx_resume", SECRET_TASK)
        .succeeded();
    let established = capsule.stored_session("ctx_resume");
    capsule.make_stateless();

    // `--resume` overrides `lifecycle.conversation: stateless` for one launch.
    let resumed = capsule
        .run("memory", &["--resume", "@1", "--task", RECALL_TASK])
        .succeeded();
    assert_eq!(resumed.launch()["session_mode"], "resume");
    assert_eq!(resumed.launch()["harness_session_id"], established.as_str());
    assert!(resumed.result().contains(SECRET), "{}", resumed.result());

    let compact = capsule
        .run(
            "memory",
            &[
                "--resume",
                "@1",
                "--resume-mode",
                "compact",
                "--task",
                RECALL_TASK,
            ],
        )
        .failed();
    assert!(
        compact.text.contains("error[E-RUN-037]"),
        "compaction has no history to work on under this transport:\n{}",
        compact.text
    );

    // With the map gone there is nothing to continue, and the refusal names what it looked for.
    let map_path = capsule.map_file("ctx_resume");
    fs::remove_file(&map_path).unwrap();
    let missing = capsule
        .run("memory", &["--resume", "@1", "--task", RECALL_TASK])
        .failed();
    assert!(
        missing.text.contains("error[E-RUN-017]"),
        "{}",
        missing.text
    );
    assert!(
        missing.text.contains(&map_path.display().to_string()),
        "the refusal names the file it looked for:\n{}",
        missing.text
    );
}

// ── S8: context.record: off ───────────────────────────────────────────────────

#[test]
fn record_off_threads_inside_one_life_and_forgets_on_restart() {
    let live = LiveCapsule::start(Built::new().queueing().record_off().build(), "memory");

    live.send("m1", "ctx_off", SECRET_TASK, 1);
    live.send("m2", "ctx_off", RECALL_TASK, 2);
    assert!(
        live.result().contains(SECRET),
        "a running capsule remembers its own contexts; got: {}",
        live.result()
    );

    let launches = live.launches();
    assert_eq!(launches[0]["session_mode"], "new");
    assert_eq!(launches[1]["session_mode"], "resume");
    assert!(
        !live.capsule.conversations_root().exists(),
        "context.record: off creates nothing under ~/.murmur/conversations/"
    );

    // A fresh capsule over the same home has no memory of that context. It runs one task and
    // exits, rather than holding a door of its own beside the one above.
    let restarted = live
        .capsule
        .run(
            "memory",
            &[
                "--lifecycle-task-acceptance",
                "single",
                "--lifecycle-after-task",
                "exit",
                "--context",
                "ctx_off",
                "--task",
                RECALL_TASK,
            ],
        )
        .succeeded();
    assert_eq!(restarted.launch()["session_mode"], "new");
    assert!(!live.capsule.conversations_root().exists());
}

// ── S9: the conversation record stays inert ───────────────────────────────────

/// A seed is rejected and recorded, a hook granted `capabilities.conversation.read` sees an empty
/// page, and no `conversation.jsonl` is written: what the `http` path records, this path leaves to
/// the harness.
#[test]
fn the_conversation_record_stays_inert_under_process() {
    use common::hook_wat::{
        conversation_reading_task_end_hook_wasm, create_hook_zip, seed_hook_wasm,
    };

    const SEEDED: &str = "remembered from somewhere else";

    let capsule = Built::new()
        .hooks(
            "  - name: seed-hook\n    version: 0.1.0\n    runtime: hook\n  - name: read-hook\n    \
             version: 0.1.0\n    runtime: hook\n    capabilities:\n      conversation:\n        \
             read: true\n",
        )
        .build();

    let artifacts = TempDir::new().unwrap();
    for (name, binding, policy, wasm) in [
        (
            "seed-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[("user", SEEDED)]),
        ),
        (
            "read-hook",
            "on-task-end",
            "reopen-task",
            conversation_reading_task_end_hook_wasm(),
        ),
    ] {
        let artifact = create_hook_zip(artifacts.path(), name, binding, policy, &wasm);
        common::publish_local(&capsule.home, &artifact).success();
    }

    let run = capsule.task("memory", "ctx_inert", SECRET_TASK).succeeded();

    // The reader reopens the task once, so `on-task-start` fires twice; every seed is refused.
    let seeds = run.of_type("context_seed");
    assert!(!seeds.is_empty(), "the seed is recorded, not dropped");
    for seed in &seeds {
        assert_eq!(seed["outcome"], "rejected", "{seed:#?}");
        assert_eq!(seed["reason"], "unsupported_transport", "{seed:#?}");
    }
    assert!(
        !run.result().contains(SEEDED),
        "nothing put the seed in front of the harness: {}",
        run.result()
    );

    // The reader reports its whole page as the reopen reason; an empty page is an empty report.
    let reopened = run.of_type("task_reopened");
    assert_eq!(reopened.len(), 1, "the reader reopens once: {reopened:#?}");
    assert_eq!(
        reopened[0]["reason"], "",
        "a hook granted conversation.read sees an empty page on this transport"
    );

    assert!(
        record_files(&capsule.conversations_root()).is_empty(),
        "no conversation.jsonl anywhere under ~/.murmur/conversations/"
    );
    assert!(
        capsule.map_file("ctx_inert").exists(),
        "the one thing this transport records is the session id"
    );
}
