//! `mur new --roster <NAME>`: the formation it writes is admitted and launched by
//! `mur run --roster` exactly as written, and every refusal leaves the directory as it found it.
//!
//! Every case runs the real `mur` binary in an empty scratch directory, under a scratch `HOME`,
//! with no provider key in its environment. Each scaffold takes its driver version from the
//! committed copy of a default-artifacts release's index, named with `--index`, unless the case
//! is about where that version comes from.

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
use common::github_release::MockGitHub;
use common::ScriptedServer;
use murmur_artifact::{
    current_platform, version_fit, ApiKeyReference, ArtifactIndex, FitSubject, RuntimeManifest,
    VersionFit,
};
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

    /// `mur new --roster <name> --index <the committed index>`.
    fn new_roster(&self, name: &str) -> Finished {
        self.run(&["new", "--roster", name, "--index", &committed_index()])
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

/// The committed verbatim copy of a default-artifacts release's index.
fn committed_index() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/published-index/artifacts-index.json")
        .display()
        .to_string()
}

/// The version of `murmur/<name>` a scaffold over [`committed_index`] pins: the newest the index
/// lists, which fits this mur.
fn committed_version(name: &str) -> String {
    let index =
        ArtifactIndex::from_json(&std::fs::read_to_string(committed_index()).unwrap()).unwrap();
    let newest = &index
        .artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .unwrap_or_else(|| panic!("the committed index lists no {name}"))
        .versions[0];
    assert_eq!(
        version_fit(
            &FitSubject::indexed(newest),
            capsule_runtime::served_wit_level(),
            current_platform()
        ),
        VersionFit::Fits,
        "{name}@{} fits this mur",
        newest.version
    );
    newest.version.clone()
}

/// The summary line naming the driver version and the index it came from.
fn driver_line(stdout: &str) -> &str {
    stdout
        .lines()
        .find(|line| line.starts_with("  Driver: "))
        .unwrap_or_else(|| panic!("no Driver: line in\n{stdout}"))
}

/// Run the printed key, driver and build-and-install steps, `steps[0..4]`, as printed. The driver
/// step is served by the fixture driver, filed in the global store under the full name and
/// version the step prints, the version label being metadata only, so the printed
/// `mur install -g` finds it there without asking a source.
fn install_as_printed(scratch: &Scratch, steps: &[String]) {
    store_key(scratch, &steps[0], "ANTHROPIC_API_KEY");
    let (driver_name, driver_version) = steps[1]
        .strip_prefix("mur install -g ")
        .unwrap_or_else(|| panic!("the second step installs the driver: {}", steps[1]))
        .split_once('@')
        .unwrap();
    assert_eq!(driver_name, "murmur/driver-anthropic");
    assert_eq!(driver_version, committed_version("driver-anthropic"));
    let artifacts = tempfile::tempdir().unwrap();
    let driver_zip = common::create_driver_artifact(
        artifacts.path(),
        driver_name,
        driver_version,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    murmur_artifact::Registry::publish(
        &murmur_artifact::LocalRegistry::new(scratch.home.path().join(".murmur/artifacts")),
        murmur_artifact::ArtifactMeta {
            name: murmur_artifact::ArtifactName::parse(driver_name).unwrap(),
            version: driver_version.to_string(),
            runtime: murmur_artifact::RuntimeType::Wasm,
            artifact_runtime: "driver".to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
            wit_contracts: None,
            owner_id: None,
            built_at_level: None,
        },
        &std::fs::read(&driver_zip).unwrap(),
    )
    .unwrap();
    for step in &steps[1..4] {
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

    let new = scratch.new_roster("crew");
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
    // Each member is named by the full name its built zip installs under, which is the name
    // `mur run --roster` launches it with.
    let roster = scratch.read("crew/roster.yaml");
    let capsules: Vec<&str> = roster
        .lines()
        .filter_map(|line| line.trim().strip_prefix("capsule: "))
        .map(|rest| rest.split_whitespace().next().unwrap_or_default())
        .collect();
    assert_eq!(
        capsules,
        ["local/crew-lead", "local/crew-worker"],
        "{roster}"
    );

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

/// Every record of the one session whose records are in the records group of `workdir_root`
/// under `home`.
fn only_trace(home: &Path, workdir_root: &Path) -> Vec<Value> {
    let group = common::records_group(home, workdir_root);
    let sessions = common::formation::session_dirs(&group);
    assert_eq!(sessions.len(), 1, "{}: {sessions:?}", group.display());
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
    let new = scratch.new_roster("crew");
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
    let lead = only_trace(scratch.home.path(), &scratch.path().join("crew"));
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
    let worker = only_trace(scratch.home.path(), &worker_dir);
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
    assert!(!worker_dir.join(".murmur").exists());

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

// ── Lead's door while the formation runs ─────────────────────────────────────

/// A `mur run --roster crew --json [<args>]` launcher, every line it writes kept.
struct RosterRun {
    child: std::process::Child,
    lines: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
    /// Every stdout line read so far, in order.
    seen: Vec<Value>,
}

impl RosterRun {
    fn start(scratch: &Scratch, args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = scratch.command(&[&["run", "--roster", "crew", "--json"], args].concat());
        let mut child = command
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (line_tx, lines) = mpsc::channel::<String>();
        let out = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = line_tx.send(line);
            }
        });
        let (err_tx, stderr) = mpsc::channel::<String>();
        let err = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                eprintln!("[launcher] {line}");
                let _ = err_tx.send(line);
            }
        });
        Self {
            child,
            lines,
            stderr,
            seen: Vec::new(),
        }
    }

    /// Stdout line `index`, waiting for it.
    fn line(&mut self, index: usize) -> Value {
        while self.seen.len() <= index {
            let line = self
                .lines
                .recv_timeout(LAUNCH_LIMIT)
                .unwrap_or_else(|_| panic!("no stdout line {index}; so far {:?}", self.seen));
            self.seen
                .push(serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}")));
        }
        self.seen[index].clone()
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for the launcher to exit, and returns its exit code and everything on its stderr.
    fn finish(&mut self) -> (Option<i32>, String) {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "the launcher did not exit");
            thread::sleep(Duration::from_millis(50));
        };
        let mut stderr: Vec<String> = Vec::new();
        while let Ok(line) = self.stderr.recv_timeout(Duration::from_millis(500)) {
            stderr.push(line);
        }
        (status.code(), stderr.join("\n"))
    }
}

impl Drop for RosterRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The operator token of `session`, as `mur token <session>` prints it.
fn operator_token(scratch: &Scratch, session: &str) -> String {
    let token = scratch.run(&["token", session]);
    token.assert_code(0);
    token.stdout.trim().to_string()
}

/// `mur ps --json`, parsed.
fn ps_json(scratch: &Scratch) -> Value {
    let ps = scratch.run(&["ps", "--json"]);
    ps.assert_code(0);
    serde_json::from_str(&ps.stdout).unwrap_or_else(|_| panic!("not JSON: {}", ps.stdout))
}

/// `formation_id`'s entry in a `mur ps --json` listing.
fn listed_formation<'a>(listing: &'a Value, formation_id: &str) -> &'a Value {
    listing["formations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|formation| formation["formation_id"] == formation_id)
        .unwrap_or_else(|| panic!("{formation_id} is not listed: {listing}"))
}

/// The capsule entry of roster member `member` in a `mur ps --json` listing.
fn listed_member<'a>(listing: &'a Value, member: &str) -> &'a Value {
    listing["capsules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|capsule| capsule["formation_member"] == member)
        .unwrap_or_else(|| panic!("{member} is not listed: {listing}"))
}

/// The tasks a `ListTasks` on `door` answers with.
fn list_tasks(door: &str, token: &str) -> Vec<Value> {
    let listed = common::door_capsule::rpc(door, Some(token), "ListTasks", serde_json::json!({}));
    assert_eq!(listed.status, 200, "{listed:?}");
    listed.json()["result"]["tasks"].as_array().unwrap().clone()
}

/// Whether a process with `pid` exists.
fn pid_exists(pid: u64) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The scaffold, launched with `--task`: while lead's first model request is held, lead's door
/// lists the formation's task as working and a stream on it ends completed, and `mur ps --json`
/// marks lead as the formation's entry member. The launcher then exits 0 and nothing remains.
#[test]
fn the_entry_members_launch_task_is_on_its_door_while_it_runs() {
    let scratch = Scratch::new();
    // Lead's first model request is held until the test releases it. The server records a
    // request only once it is answered, so the request says it arrived on its own channel.
    let (release, released) = mpsc::channel::<()>();
    let (arrive, arrived) = mpsc::channel::<()>();
    let server = ScriptedServer::start_answering(1, move |_| {
        let _ = arrive.send(());
        let _ = released.recv_timeout(LAUNCH_LIMIT);
        end_turn(1, "hello back")
    });
    scratch.write_config(&format!(
        "inference:\n  provider: anthropic\n  model: test-model\n  endpoint: {}\n",
        server.endpoint
    ));
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    install_as_printed(&scratch, &next_steps(&new.stdout));
    for member in ["lead", "worker"] {
        let manifest = scratch.read(&format!("crew/{member}/murmur.yaml"));
        assert!(!manifest.contains("max_tokens"), "{member}: {manifest}");
    }

    let _lock = launch_lock();
    let mut run = RosterRun::start(&scratch, &["--task", "hello"], &[]);
    let formation = run.line(0);
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = run.line(1);
    let door = readiness["url"].as_str().unwrap().to_string();
    let session = readiness["session_id"].as_str().unwrap().to_string();
    let token = operator_token(&scratch, &session);
    arrived
        .recv_timeout(LAUNCH_LIMIT)
        .expect("lead's first model request never arrived");

    let tasks = list_tasks(&door, &token);
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0]["status"]["state"], "TASK_STATE_WORKING");
    let task_id = tasks[0]["id"].as_str().unwrap().to_string();

    let listing = ps_json(&scratch);
    let lead = listed_member(&listing, "lead");
    assert_eq!(lead["session_id"], session.as_str());
    assert_eq!(lead["formation_entry"], true, "{listing}");
    assert_eq!(listed_member(&listing, "worker")["formation_entry"], false);
    assert_eq!(
        listed_formation(&listing, &formation_id)["entry_member"],
        "lead",
        "{listing}"
    );

    let (subscribed_door, subscriber_token, followed) =
        (door.clone(), token.clone(), task_id.clone());
    let subscription = thread::spawn(move || {
        common::stream::subscribe(
            &subscribed_door,
            Some(&subscriber_token),
            &followed,
            false,
            None,
        )
    });
    thread::sleep(Duration::from_secs(1));
    release.send(()).unwrap();
    let (response, events) = subscription.join().unwrap();
    assert_eq!(response.status, 200, "{response:?}");
    let last = events.last().expect("the stream carried events");
    assert!(last.is_terminal_status(), "{events:?}");
    assert_eq!(last.state(), Some("TASK_STATE_COMPLETED"), "{events:?}");

    let (code, stderr) = run.finish();
    assert_eq!(code, Some(0), "{stderr}");
    assert!(!stderr.contains("E-RUN-052"), "{stderr}");
    let pids: Vec<u32> = [
        formation["peers"][0]["pid"].as_u64(),
        readiness["pid"].as_u64(),
    ]
    .into_iter()
    .flatten()
    .map(|pid| pid as u32)
    .collect();
    assert_no_member_remains(scratch.path(), &pids, Duration::from_secs(30));
}

/// The scaffold, launched with no `--task`: the formation stays up past the idle timeout, lead
/// takes two tasks at its door one after the other without the formation ending, and `mur stop`
/// ends everything.
#[test]
fn a_formation_launched_without_a_task_stays_up_until_stopped() {
    let scratch = Scratch::new();
    let server = ScriptedServer::start(vec![end_turn(1, "first"), end_turn(2, "second")]);
    scratch.write_config(&format!(
        "inference:\n  provider: anthropic\n  model: test-model\n  endpoint: {}\n",
        server.endpoint
    ));
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    install_as_printed(&scratch, &next_steps(&new.stdout));

    let _lock = launch_lock();
    let mut run = RosterRun::start(&scratch, &[], &[("MURMUR_A2A_TIMEOUT_SECS", "2")]);
    let formation = run.line(0);
    let formation_id = formation["formation_id"].as_str().unwrap().to_string();
    let readiness = run.line(1);
    let door = readiness["url"].as_str().unwrap().to_string();
    let session = readiness["session_id"].as_str().unwrap().to_string();
    let lead_pid = readiness["pid"].as_u64().unwrap();
    let worker_pid = formation["peers"][0]["pid"].as_u64().unwrap();

    thread::sleep(Duration::from_secs(8));
    assert!(run.running(), "the launcher ended with no task to run");
    assert!(pid_exists(lead_pid) && pid_exists(worker_pid));
    let listing = ps_json(&scratch);
    assert_eq!(
        listed_formation(&listing, &formation_id)["entry_member"],
        "lead",
        "{listing}"
    );
    assert_eq!(listed_member(&listing, "lead")["formation_entry"], true);

    let token = operator_token(&scratch, &session);
    for (index, text) in ["first task", "second task"].into_iter().enumerate() {
        let sent = common::door_capsule::rpc(
            &door,
            Some(&token),
            "SendMessage",
            common::door_capsule::returning_immediately(common::door_capsule::message(
                &format!("m-{index}"),
                text,
            )),
        );
        assert_eq!(sent.status, 200, "{sent:?}");
        let task_id = sent.json()["result"]["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let done = common::door_capsule::wait_completed(&door, Some(&token), &task_id);
        assert_eq!(
            done["result"]["status"]["state"], "TASK_STATE_COMPLETED",
            "{done}"
        );
        thread::sleep(Duration::from_millis(500));
        assert!(run.running(), "the formation ended after task {index}");
        assert!(pid_exists(lead_pid), "lead ended after task {index}");
    }
    assert_eq!(server.requests().len(), 2);

    let stop = scratch.run(&["stop", &formation_id]);
    stop.assert_code(0);
    let (code, stderr) = run.finish();
    eprintln!("[new-roster] a no-task formation ended by mur stop exited {code:?}");
    assert_eq!(code, Some(STOPPED_FORMATION_EXIT), "{stderr}");
    assert!(!stderr.contains("E-RUN-052"), "{stderr}");
    assert_no_member_remains(
        scratch.path(),
        &[lead_pid as u32, worker_pid as u32],
        Duration::from_secs(30),
    );
}

/// The launcher's exit code when `mur stop <frm_id>` ends a formation launched without a task.
const STOPPED_FORMATION_EXIT: i32 = 143;

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
    let new = scratch.new_roster("crew");
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
            "{preamble}  lead     local/crew-lead@0.1.0     entry   refuses peers   authenticated door
  worker   local/crew-worker@0.1.0           serves peers    authenticated door
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
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    install_as_printed(&scratch, &next_steps(&new.stdout));

    let _lock = launch_lock();
    let launch = scratch.run(&["run", "--roster", "crew", "--json", "--task", "hello"]);
    launch.assert_code(0);
    assert!(!launch.stderr.contains("W-SEC-027"), "{}", launch.stderr);
    let formation: Value = serde_json::from_str(launch.stdout.lines().next().unwrap()).unwrap();
    let formation_id = formation["formation_id"].as_str().unwrap();

    let crew = scratch.path().join("crew");
    let lead_records = common::records_group(scratch.home.path(), &crew);
    let lead = only_trace(scratch.home.path(), &crew);
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

    let mut resume = scratch.command(&[
        "run",
        "--capsule",
        "local/crew-lead",
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
    let trace = common::read_whole_trace(&lead_records.join(session).join("trace.jsonl"));
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
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    for member in ["lead", "worker"] {
        let manifest = scratch.manifest(member);
        let driver = &manifest.artifacts[0];
        assert_eq!(driver.name, "murmur/driver-anthropic");
        assert_eq!(driver.version, committed_version("driver-anthropic"));
        assert_eq!(driver.version, "0.10.0");
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
    assert_eq!(
        driver_line(&new.stdout),
        format!(
            "  Driver: murmur/driver-anthropic@0.10.0, the newest that fits this mur in {}",
            committed_index()
        )
    );
    assert_eq!(
        next_steps(&new.stdout)[1],
        "mur install -g murmur/driver-anthropic@0.10.0"
    );
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
/// The key step writes the first config file. The `murmur` scope's source is built in, so the
/// file names no source and the `mur install -g` step after it still reaches the default
/// artifacts.
fn the_key_step_comes_first_and_writes_no_registry_source() {
    let scratch = Scratch::new();
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    let steps = next_steps(&new.stdout);
    assert!(steps[1].starts_with("mur install -g "), "{steps:?}");
    assert!(steps[1].contains("murmur/"), "{steps:?}");
    assert!(!scratch.config_path().exists());
    store_key(&scratch, &steps[0], "ANTHROPIC_API_KEY");
    let config: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(scratch.config_path()).unwrap()).unwrap();
    assert!(config["registry"]["sources"].is_null(), "{config:?}");
    assert!(config["registry"]["default"].is_null(), "{config:?}");
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
    let new = scratch.new_roster("crew");
    new.assert_code(0);
    for member in ["lead", "worker"] {
        let manifest = scratch.manifest(member);
        let driver = &manifest.artifacts[0];
        assert_eq!(driver.name, "murmur/driver-openai");
        assert_eq!(driver.version, committed_version("driver-openai"));
        assert_eq!(driver.version, "0.9.0");
        let gateway = driver.gateway.as_ref().unwrap();
        assert_eq!(gateway.endpoint, "https://api.openai.com/v1");
        assert_eq!(
            gateway.api_key,
            Some(ApiKeyReference::Environment("OPENAI_API_KEY".to_string()))
        );
        assert_eq!(manifest.inference.unwrap().model, "gpt-5.6-luna");
    }
    let steps = next_steps(&new.stdout);
    assert_eq!(steps[1], "mur install -g murmur/driver-openai@0.9.0");
    assert_eq!(
        driver_line(&new.stdout),
        format!(
            "  Driver: murmur/driver-openai@0.9.0, the newest that fits this mur in {}",
            committed_index()
        )
    );
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
    let new = scratch.new_roster("crew");
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
    let new = scratch.new_roster("crew");
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
    let new = scratch.new_roster("crew");
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
    let new = scratch.new_roster("crew");
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
    let new = scratch.new_roster("crew");
    new.assert_code(1);
    assert!(new.stderr.contains("error[E-NEW-003]"), "{}", new.stderr);
    assert!(new.stderr.contains("./crew"), "{}", new.stderr);
    assert_eq!(scratch.tree(), before);
}

#[test]
fn a_second_scaffold_of_the_same_name_is_refused() {
    let scratch = Scratch::new();
    scratch.new_roster("crew").assert_code(0);
    let before = scratch.tree();
    let again = scratch.new_roster("crew");
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
        let new = scratch.run(&[
            "new",
            &format!("--roster={name}"),
            "--index",
            &committed_index(),
        ]);
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

// ── Where the driver version comes from ──────────────────────────────────────

/// The default-artifacts repository and its owner's GitHub id, as the built-in source checks.
const DEFAULT_REPO: &str = "murmur-nexus/default-artifacts";
const MURMUR_OWNER_ID: u64 = 190_196_424;

/// An interface list naming a package version this mur does not serve, sorted.
const UNSERVED: &str =
    r#"["murmur:driver/process@9.9.9", "murmur:stream/events@0.1.0", "murmur:tool/run@0.1.0"]"#;

/// Interfaces this mur serves.
const SERVED: &str = r#"["murmur:stream/events@0.1.0", "murmur:tool/run@0.1.0"]"#;

/// A schema 2 index of `scope` listing `name` at each `(version, interfaces)`, newest first.
fn index_json(scope: &str, name: &str, versions: &[(&str, &str)]) -> String {
    let versions: Vec<String> = versions
        .iter()
        .map(|(version, interfaces)| {
            format!(
                r#"{{"version": "{version}", "release": "v0.99.0", "interfaces": {interfaces},
                    "assets": [{{"file": "{name}-{version}.mur.zip", "sha256": "{}"}}]}}"#,
                "ab".repeat(32)
            )
        })
        .collect();
    format!(
        r#"{{"schema_version": 2, "scope": "{scope}", "artifacts": [
            {{"name": "{name}", "runtime": "driver", "tags": [], "versions": [{}]}}]}}"#,
        versions.join(", ")
    )
}

/// With no `--index`, the version is the newest that fits this mur in the index on the latest
/// default-artifacts release, read through the built-in source: a newer version needing an
/// interface this mur does not serve is passed over, and no payload is downloaded.
#[test]
fn the_default_source_decides_the_driver_version_by_fit() {
    let scratch = Scratch::new();
    let stub = MockGitHub::start();
    stub.repository(DEFAULT_REPO, MURMUR_OWNER_ID);
    let index = index_json(
        "murmur",
        "driver-anthropic",
        &[("0.12.0", UNSERVED), ("0.11.0", SERVED)],
    );
    stub.release(
        DEFAULT_REPO,
        "v0.99.0",
        vec![("artifacts-index.json".to_string(), index.into_bytes())],
    );
    let output = scratch
        .command(&["new", "--roster", "crew"])
        .env("MUR_GITHUB_API_BASE", &stub.api_base)
        .env_remove("GITHUB_TOKEN")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for member in ["lead", "worker"] {
        let manifest = scratch.manifest(member);
        assert_eq!(manifest.artifacts[0].name, "murmur/driver-anthropic");
        assert_eq!(manifest.artifacts[0].version, "0.11.0", "{member}");
    }
    assert_eq!(
        next_steps(&stdout)[1],
        "mur install -g murmur/driver-anthropic@0.11.0"
    );
    assert_eq!(
        driver_line(&stdout),
        "  Driver: murmur/driver-anthropic@0.11.0, the newest that fits this mur in \
         github:murmur-nexus/default-artifacts v0.99.0"
    );
    let paths = stub.paths();
    assert!(
        !paths.is_empty()
            && paths
                .iter()
                .all(|path| path.starts_with("/repos/murmur-nexus/default-artifacts")),
        "{paths:?}"
    );
    assert!(!stub.downloaded_a_payload(), "{paths:?}");
    assert!(
        !scratch.config_path().exists(),
        "mur new --roster wrote a config"
    );
}

/// A loopback address nothing listens on.
fn closed_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(listener);
    address
}

/// Fail unless `finished` refused with `code`, said each of `said`, and left the scratch
/// directory empty.
fn assert_refused(scratch: &Scratch, finished: &Finished, code: &str, said: &[&str]) {
    finished.assert_code(1);
    let refusal = error_line(&finished.stderr);
    assert!(
        refusal.starts_with(&format!("error[{code}]")),
        "{}",
        finished.stderr
    );
    for said in said {
        assert!(
            finished.stderr.contains(said),
            "{said}: {}",
            finished.stderr
        );
    }
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
}

/// No index to read, whether the default source, an `--index` URL or an `--index` file: refused
/// `E-NEW-006`, naming what was not read and `--index`, with nothing written.
#[test]
fn no_index_reachable_is_refused_naming_index() {
    let scratch = Scratch::new();
    let api_base = format!("http://{}", closed_address());
    let output = scratch
        .command(&["new", "--roster", "crew"])
        .env("MUR_GITHUB_API_BASE", &api_base)
        .env_remove("GITHUB_TOKEN")
        .output()
        .unwrap();
    let source = Finished {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_refused(
        &scratch,
        &source,
        "E-NEW-006",
        &[
            "could not read the artifact index of github:murmur-nexus/default-artifacts:",
            "hint: pass --index <PATH|URL>",
        ],
    );

    let url = format!("http://{}/artifacts-index.json", closed_address());
    let fetched = scratch.run(&["new", "--roster", "crew", "--index", &url]);
    assert_refused(
        &scratch,
        &fetched,
        "E-NEW-006",
        &[
            &format!("could not read the artifact index at {url}:"),
            "hint: pass --index <PATH|URL>",
        ],
    );

    let missing = scratch.run(&["new", "--roster", "crew", "--index", "./missing.json"]);
    assert_refused(
        &scratch,
        &missing,
        "E-NEW-006",
        &[
            "could not read the artifact index file ./missing.json:",
            "hint: pass --index <PATH|URL>",
        ],
    );
}

/// An index with no fitting version of the driver: refused `E-NEW-007` naming the driver and the
/// index, and for versions that do not fit, the newest and why. Nothing is written.
#[test]
fn an_index_with_no_fitting_driver_is_refused() {
    let scratch = Scratch::new();
    let files = tempfile::tempdir().unwrap();
    let write = |file: &str, text: &str| {
        let path = files.path().join(file);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let unlisted = write(
        "unlisted.json",
        &index_json("murmur", "driver-openai", &[("0.9.0", SERVED)]),
    );
    let unfit = write(
        "unfit.json",
        &index_json("murmur", "driver-anthropic", &[("0.12.0", UNSERVED)]),
    );
    let foreign = write(
        "foreign.json",
        &index_json("ana", "driver-anthropic", &[("0.10.0", SERVED)]),
    );
    for (index, said) in [
        (&unlisted, vec!["lists no murmur/driver-anthropic"]),
        (
            &unfit,
            vec![
                "lists no version of murmur/driver-anthropic that fits this mur",
                "the newest it lists, 0.12.0, needs",
                "murmur:driver/process@9.9.9",
            ],
        ),
        (&foreign, vec!["'ana' scope", "murmur/driver-anthropic"]),
    ] {
        let refused = scratch.run(&["new", "--roster", "crew", "--index", index]);
        let mut said = said;
        said.push(index.as_str());
        said.push("hint: upgrade mur, or pass --index <PATH|URL>");
        assert_refused(&scratch, &refused, "E-NEW-007", &said);
    }

    let old = write("old.json", r#"{"schema_version":"1","artifacts":[]}"#);
    let refused = scratch.run(&["new", "--roster", "crew", "--index", &old]);
    assert_refused(&scratch, &refused, "E-IDX-002", &[old.as_str()]);
}

/// `--index local` is refused before the config is read: a malformed config is never reached.
#[test]
fn a_local_index_is_refused_before_the_config_is_read() {
    let scratch = Scratch::new();
    let config = "inference: [this is not a mapping\n";
    scratch.write_config(config);
    let refused = scratch.run(&["new", "--roster", "crew", "--index", "local"]);
    assert_refused(
        &scratch,
        &refused,
        "E-NEW-005",
        &[
            "--index local",
            "hint: pass an index file or an http(s) URL",
        ],
    );
    assert!(!refused.stderr.contains("E-CFG"), "{}", refused.stderr);
    assert_eq!(
        std::fs::read_to_string(scratch.config_path()).unwrap(),
        config
    );
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
    assert!(
        help.stdout.contains("--index <PATH|URL>"),
        "{}",
        help.stdout
    );

    let task = scratch.run(&["new", "summarise a doc"]);
    task.assert_code(2);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());

    scratch.new_roster("crew").assert_code(0);
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_roster_form_conflicts_with_the_task_form() {
    let scratch = Scratch::new();
    let conflict = scratch.run(&["new", "--roster", "crew", "summarise a doc"]);
    conflict.assert_code(2);
    assert!(conflict.stderr.contains("--roster"), "{}", conflict.stderr);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_task_form_refuses_registry_and_names_index() {
    let scratch = Scratch::new();
    let refused = scratch.run(&["new", "summarise a doc", "--registry", "local"]);
    refused.assert_code(2);
    assert!(
        refused.stderr.contains("unexpected argument '--registry'"),
        "{}",
        refused.stderr
    );
    assert!(
        refused
            .stderr
            .contains("hint: to choose the artifact index, pass --index <PATH|URL>"),
        "{}",
        refused.stderr
    );
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
}

/// A scratch with `mur-new` enabled and nothing else: no `inference:`, no store, no provider
/// key, and no terminal on stdin.
#[cfg(feature = "beta-mur-new")]
fn beta_task_scratch() -> Scratch {
    let scratch = Scratch::new();
    scratch.run(&["beta", "enable", "mur-new"]).assert_code(0);
    scratch
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_task_form_refuses_a_local_index_before_reading_any_configuration() {
    let scratch = beta_task_scratch();
    let refused = scratch.run(&["new", "a capsule that greps logs", "--index", "local"]);
    refused.assert_code(1);
    assert!(
        refused.stderr.contains("error[E-NEW-005]"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("hint:")
            && refused.stderr.contains("mur search <query> --index local"),
        "{}",
        refused.stderr
    );
    assert!(!refused.stderr.contains("E-CFG-001"), "{}", refused.stderr);
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
    assert!(
        !scratch
            .home
            .path()
            .join(".murmur")
            .join("artifacts")
            .exists(),
        "nothing is staged"
    );
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_task_form_checks_an_index_file_on_the_host_first() {
    let scratch = beta_task_scratch();

    let missing = scratch.run(&[
        "new",
        "a capsule that greps logs",
        "--index",
        "./missing.json",
    ]);
    missing.assert_code(1);
    assert!(
        missing.stderr.contains("error[E-NEW-006]") && missing.stderr.contains("./missing.json"),
        "{}",
        missing.stderr
    );
    assert!(!missing.stderr.contains("E-CFG-001"), "{}", missing.stderr);

    std::fs::write(
        scratch.path().join("old.json"),
        r#"{"schema_version":"1","artifacts":[]}"#,
    )
    .unwrap();
    let old = scratch.run(&["new", "a capsule that greps logs", "--index", "./old.json"]);
    old.assert_code(1);
    assert!(old.stderr.contains("E-IDX-002"), "{}", old.stderr);
    assert!(
        old.stderr
            .contains("hint: this is a schema 1 artifact index, which this mur does not read"),
        "{}",
        old.stderr
    );
    assert!(!old.stderr.contains("E-CFG-001"), "{}", old.stderr);
    assert!(!scratch.path().join("murmur.yaml").exists());
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn the_task_form_names_a_global_install_for_a_missing_generator_artifact() {
    let scratch = beta_task_scratch();
    let output = scratch
        .command(&[
            "new",
            "a capsule that greps logs",
            "--index",
            &committed_index(),
        ])
        .env("OPENAI_API_KEY", ["sk-", "test"].concat())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("error[E-RUN-008]"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "hint: run `mur install -g murmur/driver-openai@{}`",
            committed_version("driver-openai")
        )),
        "{stderr}"
    );
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

    scratch.new_roster("crew").assert_code(0);
}

/// The generator's tools take their versions from the same index: one that lists no
/// `murmur/tool-editor` is refused `E-NEW-007` before anything is staged.
#[cfg(feature = "beta-mur-new")]
#[test]
fn the_task_form_refuses_an_index_with_no_fitting_generator_tool() {
    let scratch = beta_task_scratch();
    let mut index: Value =
        serde_json::from_str(&std::fs::read_to_string(committed_index()).unwrap()).unwrap();
    index["artifacts"]
        .as_array_mut()
        .unwrap()
        .retain(|artifact| artifact["name"] != "tool-editor");
    let path = scratch.home.path().join("no-editor-index.json");
    std::fs::write(&path, serde_json::to_string_pretty(&index).unwrap()).unwrap();
    // The whole config: the beta flag `beta_task_scratch` set, and a complete inference block.
    scratch.write_config(&format!(
        "beta:\n  enabled:\n  - mur-new\ninference:\n  provider: openai\n  model: gpt-5.6-luna\n  \
         api_key: {}\n",
        fake_key()
    ));
    let refused = scratch.run(&[
        "new",
        "a capsule that greps logs",
        "--index",
        path.to_str().unwrap(),
    ]);
    refused.assert_code(1);
    assert!(
        refused.stderr.contains("error[E-NEW-007]"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("murmur/tool-editor")
            && refused.stderr.contains(path.to_str().unwrap()),
        "{}",
        refused.stderr
    );
    assert!(scratch.tree().is_empty(), "{:?}", scratch.tree());
    assert!(
        !scratch.home.path().join(".murmur/artifacts").exists(),
        "nothing is staged"
    );
}
