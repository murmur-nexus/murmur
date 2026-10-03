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
        let output = self.command(args).output().unwrap();
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

    // The driver step names the driver and version the members pin; the fixture driver is
    // published under that name and version, the version label being metadata only.
    let driver = steps[0]
        .strip_prefix("mur install -g ")
        .unwrap_or_else(|| panic!("the first step installs the driver: {}", steps[0]));
    let (driver_name, driver_version) = driver.split_once('@').unwrap();
    assert_eq!(driver_name, "murmur-driver-anthropic");
    let artifacts = tempfile::tempdir().unwrap();
    let driver_zip = common::create_driver_artifact(
        artifacts.path(),
        driver_name,
        driver_version,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&scratch.home, &driver_zip).success();

    // The build-and-install steps, run as printed.
    for step in &steps[1..3] {
        for command in step.split("&&") {
            let words: Vec<&str> = command.split_whitespace().collect();
            assert_eq!(words[0], "mur", "{step}");
            scratch.run(&words[1..]).assert_code(0);
        }
    }
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

    assert_eq!(steps[3], "export ANTHROPIC_API_KEY=...");
    assert_eq!(steps[4], "mur run --roster crew --task \"<your task>\"");
    assert_eq!(steps.len(), 5, "{steps:?}");

    let _lock = launch_lock();
    let mut launcher = scratch
        .command(&["run", "--roster", "crew", "--json", "--task", "hello"])
        .env("ANTHROPIC_API_KEY", "test-key")
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
    assert!(
        status.success(),
        "the launcher failed: {status}; stderr:\n{}",
        stderr.try_iter().collect::<Vec<_>>().join("\n")
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
    assert!(new.stdout.contains("export ANTHROPIC_API_KEY=..."));
    assert!(
        !scratch.config_path().exists(),
        "mur new --roster wrote a config"
    );
    assert!(!scratch.home.path().join(".murmur").exists());
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
        assert_eq!(gateway.endpoint, "https://api.openai.com");
        assert_eq!(
            gateway.api_key,
            Some(ApiKeyReference::Environment("OPENAI_API_KEY".to_string()))
        );
        assert_eq!(manifest.inference.unwrap().model, "gpt-4o-mini");
    }
    let steps = next_steps(&new.stdout);
    assert!(steps[0].starts_with("mur install -g murmur-driver-openai@"));
    assert!(steps.contains(&"export OPENAI_API_KEY=...".to_string()));
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

    scratch.run(&["new", "--roster", "crew"]).assert_code(0);
}
