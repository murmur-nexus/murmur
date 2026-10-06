//! `mur new --roster <NAME>`: the formation it writes is admitted and launched by
//! `mur run --roster` exactly as written, and every refusal leaves the directory as it found it.
//!
//! Every case runs the real `mur` binary in an empty scratch directory, under a scratch `HOME`,
//! with no provider key in its environment.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::door_capsule::end_turn;
use common::formation::{assert_no_member_remains, launch_lock};
use common::ScriptedServer;
use murmur_artifact::{ApiKeyReference, RuntimeManifest};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The files `mur new --roster crew` writes, relative to the directory it runs in.
const CREW_FILES: [&str; 3] = [
    "crew/roster.yaml",
    "crew/lead/murmur.yaml",
    "crew/worker/murmur.yaml",
];

/// How long the launch may take end to end before the test gives up on it.
const LAUNCH_LIMIT: Duration = Duration::from_secs(240);

/// A scratch `HOME` and an empty directory to run `mur new` in.
struct Scratch {
    home: TempDir,
    dir: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            dir: tempfile::Builder::new()
                .prefix("new-roster-")
                .tempdir()
                .unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".murmur").join("config.yaml")
    }

    fn write_config(&self, yaml: &str) {
        std::fs::create_dir_all(self.config_path().parent().unwrap()).unwrap();
        std::fs::write(self.config_path(), yaml).unwrap();
    }

    /// `mur <args>` in the scratch directory, with no provider key, door token or formation
    /// inherited from this process.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(args)
            .current_dir(self.path())
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .env_remove("MURMUR_SPAWNER");
        command
    }

    fn run(&self, args: &[&str]) -> Finished {
        self.run_in(self.path(), args)
    }

    /// `mur <args>` as [`Scratch::command`] runs it, from `dir`.
    fn run_in(&self, dir: &Path, args: &[&str]) -> Finished {
        let output = self.command(args).current_dir(dir).output().unwrap();
        Finished {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Every path under the scratch directory, relative to it, with a file's sha256 or `dir`.
    fn tree(&self) -> BTreeMap<String, String> {
        let mut tree = BTreeMap::new();
        let mut stack = vec![self.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let relative = path
                    .strip_prefix(self.path())
                    .unwrap()
                    .display()
                    .to_string();
                if path.is_dir() {
                    tree.insert(relative, "dir".to_string());
                    stack.push(path);
                } else {
                    tree.insert(relative, sha256(&path));
                }
            }
        }
        tree
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path().join(relative)).unwrap()
    }

    fn manifest(&self, member: &str) -> RuntimeManifest {
        RuntimeManifest::from_yaml_str(&self.read(&format!("crew/{member}/murmur.yaml"))).unwrap()
    }
}

struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Finished {
    fn assert_code(&self, code: i32) -> &Self {
        assert_eq!(
            self.code,
            Some(code),
            "stdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr
        );
        self
    }
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

/// The lines of the `Next:` block `mur new --roster` printed, without their indentation.
fn next_steps(stdout: &str) -> Vec<String> {
    stdout
        .split_once("\nNext:\n")
        .unwrap_or_else(|| panic!("no Next: block in\n{stdout}"))
        .1
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// A key-shaped value built at runtime, so the source holds no credential-shaped literal.
fn fake_key() -> String {
    ["sk-", "scaffold-", "never-written"].concat()
}

/// Fail if any generated file, or `stdout`, carries the key `key`.
fn assert_no_key_written(scratch: &Scratch, stdout: &str, key: &str) {
    for file in CREW_FILES {
        assert!(!scratch.read(file).contains(key), "{file} holds the key");
    }
    assert!(!stdout.contains(key), "stdout holds the key");
}

/// The test key the printed key step stores in place of `<your key>`.
const TEST_KEY: &str = "test-key";

/// Run the printed key step as printed, `<your key>` replaced by `TEST_KEY`, and check it stored
/// the key in the scratch config without echoing it.
fn store_key(scratch: &Scratch, step: &str, key_var: &str) {
    assert_eq!(
        step,
        format!("mur config set -g credentials.{key_var} <your key>")
    );
    let typed = step.replace("<your key>", TEST_KEY);
    let words: Vec<&str> = typed.split_whitespace().collect();
    assert_eq!(words[0], "mur", "{step}");
    let set = scratch.run(&words[1..]);
    set.assert_code(0);
    assert!(
        set.stdout.contains(&format!(
            "Set credentials.{key_var} in ~/.murmur/config.yaml"
        )),
        "{}",
        set.stdout
    );
    assert!(!set.stdout.contains(TEST_KEY) && !set.stderr.contains(TEST_KEY));
}

/// Run the printed key, driver and build-and-install steps, `steps[0..4]`, as printed. The driver
/// step is served by the fixture driver, published locally under the name and version the step
/// prints, the version label being metadata only.
fn install_as_printed(scratch: &Scratch, steps: &[String]) {
    store_key(scratch, &steps[0], "ANTHROPIC_API_KEY");
    let (driver_name, driver_version) = steps[1]
        .strip_prefix("mur install -g ")
        .unwrap_or_else(|| panic!("the second step installs the driver: {}", steps[1]))
        .split_once('@')
        .unwrap();
    assert_eq!(driver_name, "murmur-driver-anthropic");
    let artifacts = tempfile::tempdir().unwrap();
    let driver_zip = common::create_driver_artifact(
        artifacts.path(),
        driver_name,
        driver_version,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&scratch.home, &driver_zip).success();
    for step in &steps[2..4] {
        for command in step.split("&&") {
            let words: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(words[0], "mur", "{step}");
            scratch.run(&words[1..]).assert_code(0);
        }
    }
}

// ── S1: the scaffold launches unedited ───────────────────────────────────────

#[test]
fn the_scaffold_installs_and_launches_as_printed() {
    let scratch = Scratch::new();
    let server = ScriptedServer::start(vec![end_turn(1, "hello back")]);
    scratch.write_config(&format!(
        "inference:\n  provider: anthropic\n  model: test-model\n  endpoint: {}\n",
        server.endpoint
    ));

    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    let steps = next_steps(&new.stdout);
    let hashes: Vec<String> = CREW_FILES
        .iter()
        .map(|file| sha256(&scratch.path().join(file)))
        .collect();

    // The key reaches the members only through the config the key step writes.
    install_as_printed(&scratch, &steps);
    // In `tree`'s order: sorted by path.
    let expected = [
        "crew",
        "crew/lead",
        "crew/lead/crew-lead-0.1.0.mur.zip",
        "crew/lead/murmur.yaml",
        "crew/roster.yaml",
        "crew/worker",
        "crew/worker/crew-worker-0.1.0.mur.zip",
        "crew/worker/murmur.yaml",
    ];
    let tree = scratch.tree();
    assert_eq!(
        tree.keys().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "the formation directory holds its files and the two built zips"
    );

    assert_eq!(steps[4], "mur run --roster crew --task \"<your task>\"");
    assert_eq!(steps.len(), 5, "{steps:?}");

    let _lock = launch_lock();
    let mut launcher = scratch
        .command(&["run", "--roster", "crew", "--json", "--task", "hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (line_tx, lines) = mpsc::channel::<String>();
    let out = launcher.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let (err_tx, stderr) = mpsc::channel::<String>();
    let err = launcher.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            eprintln!("[launcher] {line}");
            let _ = err_tx.send(line);
        }
    });
    let deadline = Instant::now() + LAUNCH_LIMIT;
    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = launcher.kill();
            panic!(
                "the launcher did not exit; stderr:\n{}",
                stderr.try_iter().collect::<Vec<_>>().join("\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    // A member that inherited the pipe can hold it open past the launcher's exit, so the lines
    // are read until a pause rather than until end of file.
    let mut stdout: Vec<Value> = Vec::new();
    while let Ok(line) = lines.recv_timeout(Duration::from_secs(2)) {
        stdout.push(serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}")));
    }
    let mut stderr_lines: Vec<String> = Vec::new();
    while let Ok(line) = stderr.recv_timeout(Duration::from_millis(500)) {
        stderr_lines.push(line);
    }
    let stderr = stderr_lines.join("\n");
    assert!(
        status.success(),
        "the launcher failed: {status}; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("W-SEC-027"),
        "the stored key drew W-SEC-027:\n{stderr}"
    );

    let formation = &stdout[0];
    assert_eq!(formation["entry"], "lead", "{formation}");
    let peers = formation["peers"].as_array().unwrap();
    let peer_names: Vec<&str> = peers
        .iter()
        .map(|peer| peer["name"].as_str().unwrap())
        .collect();
    assert_eq!(peer_names, ["worker"], "{formation}");
    let readiness = &stdout[1];
    assert_eq!(
        readiness["formation_id"], formation["formation_id"],
        "{readiness}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["model"], "test-model");

    let mut pids: Vec<u32> = peers
        .iter()
        .filter_map(|peer| peer["pid"].as_u64())
        .map(|pid| pid as u32)
        .collect();
    pids.extend(readiness["pid"].as_u64().map(|pid| pid as u32));
    assert_no_member_remains(scratch.path(), &pids, Duration::from_secs(30));

    for (file, hash) in CREW_FILES.iter().zip(&hashes) {
        assert_eq!(&sha256(&scratch.path().join(file)), hash, "{file} changed");
    }
}

// ── The scaffold's lead hands worker its task ────────────────────────────────

/// The model of both scaffolded members, told apart by the system prompt each sends. Lead calls
/// `call-member` once, ends the turn while worker works, and answers with what came back; worker
/// answers with `WORKER-<nonce>`.
fn crew_model(nonce: &str) -> ScriptedServer {
    let nonce = nonce.to_string();
    let mut lead_turns = 0usize;
    ScriptedServer::start_answering(4, move |request| {
        let system = common::system_text(&request["system"]).unwrap_or_default();
        if system.contains("You are 'worker'") {
            return end_turn(1, &format!("WORKER-{nonce}"));
        }
        lead_turns += 1;
        match lead_turns {
            1 => common::tool_use_response(
                "toolu_call_worker",
                "call-member",
                serde_json::json!({
                    "member": "worker",
                    "task": "Reply with your token and nothing else.",
                }),
            ),
            2 => end_turn(2, "worker is on it"),
            _ => {
                let seen = request.to_string();
                let token = seen
                    .find("WORKER-")
                    .map(|at| seen[at..at + "WORKER-".len() + 32].to_string())
                    .unwrap_or_else(|| "nothing".to_string());
                end_turn(3, &format!("worker answered {token}"))
            }
        }
    })
}

/// Every record of the one session under `root`.
fn only_trace(root: &Path) -> Vec<Value> {
    let sessions = common::formation::session_dirs(root);
    assert_eq!(sessions.len(), 1, "{}: {sessions:?}", root.display());
    common::read_whole_trace(&sessions[0].join("trace.jsonl"))
}

fn records<'a>(trace: &'a [Value], kind: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|record| record["event_type"] == kind)
        .collect()
}

/// `mur new --roster crew`, every printed install step, then `mur run --roster crew`: lead hands
/// worker a task through `call-member`, worker runs it in its own directory, and the answer
/// comes back into lead's same task, fenced under `member:worker`. Nothing in the scaffold is
/// edited, and nothing real about worker's door reaches a model or a trace.
#[test]
fn the_scaffolded_lead_hands_the_worker_a_task() {
    let scratch = Scratch::new();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let server = crew_model(&nonce);
    scratch.write_config(&format!(
        "inference:\n  provider: anthropic\n  model: test-model\n  endpoint: {}\n",
        server.endpoint
    ));
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    let steps = next_steps(&new.stdout);
    let hashes: Vec<String> = CREW_FILES
        .iter()
        .map(|file| sha256(&scratch.path().join(file)))
        .collect();
    install_as_printed(&scratch, &steps);

    let _lock = launch_lock();
    let run = scratch
        .command(&[
            "run",
            "--roster",
            "crew",
            "--json",
            "--task",
            "Get worker's token.",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert_eq!(
        run.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!stderr.contains("W-SEC-027"), "{stderr}");
    for (file, hash) in CREW_FILES.iter().zip(&hashes) {
        assert_eq!(&sha256(&scratch.path().join(file)), hash, "{file} changed");
    }

    let formation: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    let formation_id = formation["formation_id"].as_str().unwrap();
    let peer = &formation["peers"][0];
    assert_eq!(peer["name"], "worker", "{formation}");
    let worker_dir = scratch
        .home
        .path()
        .join(".murmur/formations")
        .join(formation_id)
        .join("worker");
    assert_eq!(peer["workdir"], worker_dir.display().to_string());
    let door = peer["url"].as_str().unwrap().to_string();
    let readiness: Value = serde_json::from_str(stdout.lines().nth(1).unwrap()).unwrap();

    // Lead, under the roster's project.
    let lead = only_trace(&scratch.path().join("crew/.murmur"));
    let start = &records(&lead, "session_start")[0];
    assert!(
        start["tools_declared"]
            .as_array()
            .unwrap()
            .contains(&Value::from("call-member")),
        "{start}"
    );
    let call_start = records(&lead, "member_call_start");
    assert_eq!(call_start.len(), 1, "{lead:?}");
    let call_id = call_start[0]["call_id"].as_str().unwrap().to_string();
    let hex = call_id.strip_prefix("mcl_").unwrap();
    assert!(
        hex.len() == 32
            && hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{call_id}"
    );
    assert_eq!(call_start[0]["member"], "worker");
    let member_task_id = call_start[0]["member_task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let call_end = records(&lead, "member_call");
    assert_eq!(call_end.len(), 1, "{lead:?}");
    assert_eq!(call_end[0]["call_id"], call_id.as_str());
    assert_eq!(call_end[0]["status"], "completed");
    assert_eq!(call_end[0]["delivered"], true);
    assert_eq!(call_end[0]["member_task_id"], member_task_id.as_str());
    assert!(
        call_end[0]["output"]
            .as_str()
            .unwrap()
            .contains(&format!("WORKER-{nonce}")),
        "{}",
        call_end[0]
    );
    assert_eq!(records(&lead, "task_start").len(), 1, "one task in lead");
    assert_eq!(records(&lead, "task_reopened").len(), 0);

    // Worker, under its own directory.
    let worker = only_trace(&worker_dir.join(".murmur"));
    assert!(!records(&worker, "session_start")[0]["tools_declared"]
        .as_array()
        .unwrap()
        .contains(&Value::from("call-member")));
    let received = records(&worker, "a2a_task_received");
    assert_eq!(received.len(), 1, "{worker:?}");
    assert_eq!(received[0]["caller_member"], "lead");
    assert_eq!(received[0]["message_id"], format!("msg_{call_id}"));
    assert_eq!(received[0]["task_id"], member_task_id.as_str());
    let kinds = common::event_kinds(&worker);
    let task_start = kinds.iter().position(|kind| *kind == "task_start").unwrap();
    let task_end = kinds.iter().position(|kind| *kind == "task_end").unwrap();
    assert!(task_start < task_end, "{kinds:?}");
    assert_eq!(worker[task_start]["origin"], "peer");
    assert!(!scratch.path().join("crew/worker/.murmur").exists());

    // Lead's model: call-member offered with worker as its only member, and the answer fenced.
    let requests = server.requests();
    let lead_requests: Vec<&Value> = requests
        .iter()
        .filter(|request| {
            common::system_text(&request["system"])
                .is_some_and(|system| system.contains("You are 'lead'"))
        })
        .collect();
    assert_eq!(lead_requests.len(), 3, "{requests:?}");
    let tool = lead_requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "call-member")
        .expect("call-member is offered to lead");
    assert_eq!(
        tool["input_schema"]["properties"]["member"]["enum"],
        serde_json::json!(["worker"])
    );
    let third = lead_requests[2].to_string();
    let fenced = format!("<untrusted-content source=member:worker>\\nWORKER-{nonce}");
    assert!(third.contains(&fenced), "{third}");
    assert!(
        third.contains(&format!(
            "[call-member] call {call_id} to worker ended completed"
        )),
        "{third}"
    );

    // Nothing real about the door, and no token, in any request, tool result or trace line.
    let port = door.rsplit(':').next().unwrap();
    let mut texts: Vec<String> = requests.iter().map(Value::to_string).collect();
    texts.extend(lead.iter().chain(&worker).map(Value::to_string));
    for text in &texts {
        assert!(!text.contains("mft1."), "a token leaked: {text}");
        assert!(!text.contains(&door), "the door leaked: {text}");
        assert!(
            !text.contains(&format!("localhost:{port}")),
            "the door leaked: {text}"
        );
    }

    let pids: Vec<u32> = [peer["pid"].as_u64(), readiness["pid"].as_u64()]
        .into_iter()
        .flatten()
        .map(|pid| pid as u32)
        .collect();
    assert_no_member_remains(scratch.path(), &pids, Duration::from_secs(30));
}

// ── mur doctor in the formation directory ────────────────────────────────────

/// The `error[…]: …` line on `stderr`, from `error[` on.
fn error_line(stderr: &str) -> &str {
    let line = stderr
        .lines()
        .find(|line| line.contains("error[E-"))
        .unwrap_or_else(|| panic!("no error line in\n{stderr}"));
    &line[line.find("error[").unwrap()..]
}

/// `mur doctor` in `crew/` checks the formation: before the members are installed it refuses with
/// the error `mur run --roster crew` gives, and once the printed steps have run it prints the
/// admitted roster and passes.
#[test]
fn doctor_in_the_scaffolded_formation_checks_its_roster() {
    let scratch = Scratch::new();
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    let steps = next_steps(&new.stdout);
    let crew = scratch.path().join("crew").canonicalize().unwrap();
    let doctor = || scratch.run_in(&crew, &["doctor"]);
    let preamble = format!(
        "No murmur.yaml in {}: checking its roster.yaml. Run mur doctor in a member's source directory to check that member's artifacts.\n\nRoster\n  file: {}\n",
        crew.display(),
        crew.join("roster.yaml").display()
    );

    // Nothing installed: the launcher's refusal, as doctor's one error.
    let refused = doctor();
    refused.assert_code(1);
    let run = scratch.run(&["run", "--roster", "crew", "--task", "x"]);
    assert_ne!(run.code, Some(0), "{}", run.stderr);
    let refusal = error_line(&run.stderr);
    assert!(refusal.starts_with("error[E-ROS-"), "{refusal}");
    assert_eq!(error_line(&refused.stderr), refusal);
    assert!(refused.stdout.starts_with(&preamble), "{}", refused.stdout);
    let lines: Vec<&str> = refused.stdout.lines().collect();
    assert!(lines[4].starts_with("  \u{2717}  "), "{}", refused.stdout);
    let tally = lines
        .iter()
        .position(|line| *line == "0 checks passed, 1 error found.");
    let tally = tally.unwrap_or_else(|| panic!("no tally in\n{}", refused.stdout));
    assert_eq!(lines[tally + 1], "");
    assert!(lines[tally + 2].starts_with("Fix: "), "{}", refused.stdout);
    assert_eq!(lines.len(), tally + 3, "{}", refused.stdout);
    assert!(!refused.stderr.contains("E-IO-001"), "{}", refused.stderr);

    // The printed steps run: the formation is admitted.
    install_as_printed(&scratch, &steps);
    let admitted = doctor();
    admitted.assert_code(0);
    assert_eq!(
        admitted.stdout,
        format!(
            "{preamble}  lead     crew-lead@0.1.0     entry   refuses peers   authenticated door
  worker   crew-worker@0.1.0           serves peers    authenticated door
  reachability: lead \u{2192} worker

All checks passed.
"
        )
    );
    assert!(!admitted.stderr.contains("error["), "{}", admitted.stderr);
}

// ── A resumed member session is in no formation ──────────────────────────────

/// Lead's session resumed by hand with `mur run --resume @1`, after `mur run --roster crew`,
/// continues lead's conversation and belongs to no formation: no formation id, no `call-member`,
/// no lifeline.
#[test]
fn a_hand_resume_of_a_members_session_joins_no_formation() {
    let scratch = Scratch::new();
    let server = ScriptedServer::start(vec![
        end_turn(1, "lead's first answer"),
        end_turn(1, "lead's resumed answer"),
    ]);
    scratch.write_config(&format!(
        "inference:\n  provider: anthropic\n  model: test-model\n  endpoint: {}\n",
        server.endpoint
    ));
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    install_as_printed(&scratch, &next_steps(&new.stdout));

    let _lock = launch_lock();
    let launch = scratch.run(&["run", "--roster", "crew", "--json", "--task", "hello"]);
    launch.assert_code(0);
    assert!(!launch.stderr.contains("W-SEC-027"), "{}", launch.stderr);
    let formation: Value = serde_json::from_str(launch.stdout.lines().next().unwrap()).unwrap();
    let formation_id = formation["formation_id"].as_str().unwrap();

    let lead_root = scratch.path().join("crew/.murmur");
    let lead = only_trace(&lead_root);
    let lead_start = records(&lead, "session_start")[0];
    assert_eq!(lead_start["formation_id"], formation_id, "{lead_start}");
    assert!(
        lead_start["tools_declared"]
            .as_array()
            .unwrap()
            .contains(&Value::from("call-member")),
        "{lead_start}"
    );
    let lead_session = lead_start["session_id"].as_str().unwrap().to_string();
    let lead_context = records(&lead, "task_start")[0]["context_id"]
        .as_str()
        .unwrap()
        .to_string();

    let crew = scratch.path().join("crew");
    let mut resume = scratch.command(&[
        "run",
        "--capsule",
        "crew-lead",
        "--capsule-version",
        "0.1.0",
        "--workdir",
        crew.to_str().unwrap(),
        "--resume",
        "@1",
        "--json",
        "--task",
        "What did you answer last time?",
    ]);
    resume.env_remove(capsule_runtime::FORMATION_LIFELINE_ENV);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("MURMUR_FORMATION_") {
            resume.env_remove(name);
        }
    }
    let resumed = resume.output().unwrap();
    let stdout = String::from_utf8_lossy(&resumed.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&resumed.stderr).into_owned();
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!stderr.contains("W-RUN-007"), "{stderr}");
    assert!(!stderr.contains("W-SEC-027"), "{stderr}");

    let readiness: Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert!(readiness.get("formation_id").is_none(), "{readiness}");
    let session = readiness["session_id"].as_str().unwrap();
    assert_ne!(session, lead_session);
    let trace = common::read_whole_trace(&lead_root.join(session).join("trace.jsonl"));
    let start = records(&trace, "session_start")[0];
    assert_eq!(start["resumed_from"], lead_session.as_str(), "{start}");
    assert_eq!(start["context_id"], lead_context.as_str(), "{start}");
    for key in ["formation_id", "formation_member", "formation_callees"] {
        assert!(start.get(key).is_none(), "{key} on {start}");
    }
    assert!(
        !start["tools_declared"]
            .as_array()
            .unwrap()
            .contains(&Value::from("call-member")),
        "{start}"
    );
    assert!(records(&trace, "formation_ended").is_empty(), "{trace:?}");
    assert_eq!(server.requests().len(), 2, "lead answered once in each run");
}

// ── S3: the provider ─────────────────────────────────────────────────────────

#[test]
fn with_no_config_the_members_name_anthropic() {
    let scratch = Scratch::new();
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    for member in ["lead", "worker"] {
        let manifest = scratch.manifest(member);
        let driver = &manifest.artifacts[0];
        assert_eq!(driver.name, "murmur-driver-anthropic");
        let gateway = driver.gateway.as_ref().unwrap();
        assert_eq!(gateway.endpoint, "https://api.anthropic.com");
        assert_eq!(
            gateway.api_key,
            Some(ApiKeyReference::Environment(
                "ANTHROPIC_API_KEY".to_string()
            ))
        );
        assert_eq!(
            manifest.inference.unwrap().model,
            "claude-haiku-4-5-20251001"
        );
    }
    assert!(new
        .stdout
        .contains("mur config set -g credentials.ANTHROPIC_API_KEY <your key>"));
    assert!(
        !scratch.config_path().exists(),
        "mur new --roster wrote a config"
    );
    assert!(!scratch.home.path().join(".murmur").exists());
}

/// With no `~/.murmur/config.yaml`, `mur install -g <name>@<version>` has no registry source. The
/// printed key step runs before the driver install, and the config it writes names the default
/// source the install then resolves from.
#[test]
fn the_key_step_comes_first_and_writes_the_default_registry_source() {
    let scratch = Scratch::new();
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    let steps = next_steps(&new.stdout);
    assert!(steps[1].starts_with("mur install -g "), "{steps:?}");
    assert!(!scratch.config_path().exists());
    store_key(&scratch, &steps[0], "ANTHROPIC_API_KEY");
    let config: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(scratch.config_path()).unwrap()).unwrap();
    let sources = config["registry"]["sources"].as_sequence().unwrap();
    assert!(
        sources
            .iter()
            .any(|source| source["repo"].as_str() == Some("murmur-nexus/default-artifacts")),
        "{config:?}"
    );
    assert_eq!(
        config["credentials"]["ANTHROPIC_API_KEY"].as_str(),
        Some(TEST_KEY)
    );
}

#[test]
fn an_openai_config_names_the_openai_driver_and_key() {
    let scratch = Scratch::new();
    let config = format!(
        "inference:\n  provider: openai\n  api_key: {}\n",
        fake_key()
    );
    scratch.write_config(&config);
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    for member in ["lead", "worker"] {
        let manifest = scratch.manifest(member);
        let driver = &manifest.artifacts[0];
        assert_eq!(driver.name, "murmur-driver-openai");
        let gateway = driver.gateway.as_ref().unwrap();
        assert_eq!(gateway.endpoint, "https://api.openai.com/v1");
        assert_eq!(
            gateway.api_key,
            Some(ApiKeyReference::Environment("OPENAI_API_KEY".to_string()))
        );
        assert_eq!(manifest.inference.unwrap().model, "gpt-5.6-luna");
    }
    let steps = next_steps(&new.stdout);
    assert!(steps[1].starts_with("mur install -g murmur-driver-openai@"));
    assert!(steps.contains(&"mur config set -g credentials.OPENAI_API_KEY <your key>".to_string()));
    assert!(!new.stdout.contains("ANTHROPIC_API_KEY"));
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).unwrap(),
        config
    );
    assert_no_key_written(&scratch, &new.stdout, &fake_key());
}

#[test]
fn a_configured_endpoint_and_model_are_written_and_the_key_is_not() {
    let scratch = Scratch::new();
    let config = format!(
        "inference:\n  provider: anthropic\n  model: my-model\n  endpoint: http://127.0.0.1:4242\n  api_key: {}\n",
        fake_key()
    );
    scratch.write_config(&config);
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(0);
    let manifest = scratch.manifest("lead");
    assert_eq!(
        manifest.artifacts[0].gateway.as_ref().unwrap().endpoint,
        "http://127.0.0.1:4242"
    );
    assert_eq!(manifest.inference.unwrap().model, "my-model");
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).unwrap(),
        config
    );
    assert_no_key_written(&scratch, &new.stdout, &fake_key());
}

#[test]
fn an_unknown_provider_is_refused_and_nothing_is_written() {
    let scratch = Scratch::new();
    let config = "inference:\n  provider: mistral\n";
    scratch.write_config(config);
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(1);
    assert!(new.stderr.contains("error[E-NEW-004]"), "{}", new.stderr);
    assert!(new.stderr.contains("'mistral'"), "{}", new.stderr);
    assert!(
        new.stderr.contains("'anthropic' or 'openai'"),
        "{}",
        new.stderr
    );
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).unwrap(),
        config
    );
}

#[test]
fn an_endpoint_the_manifest_refuses_is_blamed_on_the_config() {
    let scratch = Scratch::new();
    scratch.write_config("inference:\n  endpoint: http://example.com\n");
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(1);
    assert!(new.stderr.contains("error[E-MAN-003]"), "{}", new.stderr);
    assert!(new.stderr.contains("inference.endpoint"), "{}", new.stderr);
    assert!(!new.stderr.contains("internal:"), "{}", new.stderr);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
}

// ── S4: an existing target ───────────────────────────────────────────────────

#[test]
fn an_existing_directory_is_never_written_into() {
    let scratch = Scratch::new();
    std::fs::create_dir(scratch.path().join("crew")).unwrap();
    std::fs::write(scratch.path().join("crew/keep.txt"), "mine").unwrap();
    let before = scratch.tree();
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(1);
    assert!(new.stderr.contains("error[E-NEW-003]"), "{}", new.stderr);
    assert!(new.stderr.contains("./crew"), "{}", new.stderr);
    assert_eq!(scratch.tree(), before);
}

#[test]
fn an_existing_file_is_never_replaced() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path().join("crew"), "mine").unwrap();
    let before = scratch.tree();
    let new = scratch.run(&["new", "--roster", "crew"]);
    new.assert_code(1);
    assert!(new.stderr.contains("error[E-NEW-003]"), "{}", new.stderr);
    assert!(new.stderr.contains("./crew"), "{}", new.stderr);
    assert_eq!(scratch.tree(), before);
}

#[test]
fn a_second_scaffold_of_the_same_name_is_refused() {
    let scratch = Scratch::new();
    scratch.run(&["new", "--roster", "crew"]).assert_code(0);
    let before = scratch.tree();
    let again = scratch.run(&["new", "--roster", "crew"]);
    again.assert_code(1);
    assert!(
        again.stderr.contains("error[E-NEW-003]"),
        "{}",
        again.stderr
    );
    assert_eq!(scratch.tree(), before);
}

// ── S5: an invalid name ──────────────────────────────────────────────────────

#[test]
fn a_name_that_is_not_an_artifact_name_is_refused() {
    let too_long = "a".repeat(95);
    let cases = [
        ("Crew", "may contain only lowercase letters, digits and '-'"),
        (
            "my_crew",
            "may contain only lowercase letters, digits and '-'",
        ),
        ("-crew", "must not start or end with '-'"),
        (
            "crew/sub",
            "may contain only lowercase letters, digits and '-'",
        ),
        ("..", "may contain only lowercase letters, digits and '-'"),
        (too_long.as_str(), "must be at most 100 characters"),
    ];
    for (name, reason) in cases {
        let scratch = Scratch::new();
        let new = scratch.run(&["new", &format!("--roster={name}")]);
        new.assert_code(1);
        assert!(
            new.stderr.contains("error[E-NEW-002]"),
            "{name}: {}",
            new.stderr
        );
        assert!(
            new.stderr.contains(&format!("'{name}'")),
            "{name}: {}",
            new.stderr
        );
        assert!(new.stderr.contains(reason), "{name}: {}", new.stderr);
        assert!(new.stderr.contains("hint:"), "{name}: {}", new.stderr);
        assert!(scratch.tree().is_empty(), "{name}: {:?}", scratch.tree());
    }
}

// ── S6: gating ───────────────────────────────────────────────────────────────

#[cfg(not(feature = "beta-mur-new"))]
#[test]
fn the_default_build_has_only_the_roster_form() {
    let scratch = Scratch::new();
    let help = scratch.run(&["new", "--help"]);
    help.assert_code(0);
    assert!(help.stdout.contains("--roster <NAME>"), "{}", help.stdout);
    assert!(!help.stdout.contains("<TASK>"), "{}", help.stdout);
    assert!(!help.stdout.contains("--registry"), "{}", help.stdout);

    let task = scratch.run(&["new", "summarise a doc"]);
    task.assert_code(2);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());

    scratch.run(&["new", "--roster", "crew"]).assert_code(0);
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_roster_form_conflicts_with_the_task_form() {
    let scratch = Scratch::new();
    scratch
        .run(&["new", "--roster", "crew", "summarise a doc"])
        .assert_code(2);
    scratch
        .run(&["new", "--roster", "crew", "--registry", "local"])
        .assert_code(2);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn with_the_beta_flag_off_the_task_form_names_the_flag_and_the_roster_form_works() {
    let scratch = Scratch::new();
    let task = scratch.run(&["new", "summarise a doc"]);
    task.assert_code(1);
    assert!(
        task.stderr.contains("mur beta enable mur-new"),
        "{}",
        task.stderr
    );
    assert!(
        !task.stderr.contains("unrecognized subcommand"),
        "{}",
        task.stderr
    );
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());

    let help = scratch.run(&["new", "--help"]);
    assert!(help.stdout.contains("--roster <NAME>"), "{}", help.stdout);
    assert!(!help.stdout.contains("<TASK>"), "{}", help.stdout);
    assert!(!help.stdout.contains("task description"), "{}", help.stdout);

    scratch.run(&["new", "--roster", "crew"]).assert_code(0);
}
