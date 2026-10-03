//! A restarted `mur-roost` counting the capsules a previous daemon left running.
//!
//! The capsules are real `mur run` agent processes under one scratch `HOME`, each writing its own
//! running record once its door is serving. The daemon is driven in process through
//! [`mur_roost::route`] with the census taken from that home, which is what `main` installs before
//! it binds.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use capsule_runtime::{formation::FormationId, SPAWN_CREDENTIAL_HEADER};
use mur_roost::{authority::SpawnAuthority, RequestHeaders, State};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// A capsule that may spawn `worker`, so a registered root can delegate.
const ROOT_BODY: &str = "artifacts: []\ncapabilities:\n  network:\n    allow: \
                         [registry.internal]\n  spawn:\n    allow: [worker]\n";
const WORKER_BODY: &str = "artifacts: []\ncapabilities:\n  network:\n    allow: \
                           [registry.internal]\n";

/// A scratch `$HOME` with the fixture inference driver published into it.
fn driver_home() -> TempDir {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", driver.to_str().unwrap()])
        .assert()
        .success();
    home
}

/// An agent capsule that sleeps between tasks, so it stays running with its door open.
fn agent_project(endpoint: &str, name: &str) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {name}\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
             capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
             lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    project
}

/// A background `mur run` process and the `--json` startup line it printed.
struct Capsule {
    child: Child,
    _project: TempDir,
    startup: Value,
}

impl Capsule {
    fn start(home: &Path, endpoint: &str, name: &str, env: &[(&str, &str)]) -> Self {
        let project = agent_project(endpoint, name);
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .args(["run", "--manifest"])
            .arg(project.path().join("murmur.yaml"))
            .arg("--json")
            .current_dir(project.path())
            .env("HOME", home)
            .env_remove("NEXUS_API_KEY")
            .env_remove("MURMUR_ROOST_URL")
            .envs(env.iter().copied())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("mur run should start");

        let (startup_tx, startup_rx) = mpsc::channel::<Value>();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if value.get("url").is_some() {
                        let _ = startup_tx.send(value);
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

        let startup = match startup_rx.recv_timeout(Duration::from_secs(120)) {
            Ok(startup) => startup,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out waiting for the capsule to print where its door is");
            }
        };
        Self {
            child,
            _project: project,
            startup,
        }
    }

    fn session_id(&self) -> String {
        self.startup["session_id"].as_str().unwrap().to_string()
    }

    fn pid(&self) -> u32 {
        self.startup["pid"].as_u64().unwrap() as u32
    }
}

impl Drop for Capsule {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn record_for(home: &Path, session_id: &str) -> PathBuf {
    home.join(".murmur")
        .join("running")
        .join(format!("{session_id}.json"))
}

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether any process holds `pid`, as `kill -0` reports it.
fn pid_is_held(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Publishes a capsule into the daemon's registry under `name@0.1.0`.
fn publish(registry: &Path, name: &str, body: &str) {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(format!("name: {name}\nversion: 0.1.0\n{body}").as_bytes())
            .unwrap();
        zip.start_file("capsule.wasm", options).unwrap();
        zip.write_all(
            &fs::read(common::fixture_path("run/components/capsule-env-echo.wasm")).unwrap(),
        )
        .unwrap();
        zip.finish().unwrap();
    }
    murmur_artifact::Registry::publish(
        &murmur_artifact::LocalRegistry::new(registry),
        murmur_artifact::ArtifactMeta {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            runtime: murmur_artifact::RuntimeType::Wasm,
            artifact_runtime: "capsule".to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
            wit_contracts: None,
        },
        &cursor.into_inner(),
    )
    .unwrap();
}

/// One request through the daemon's router: the status code and the JSON body.
fn route(
    state: &Arc<State>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, Value) {
    let mut request_headers = RequestHeaders::new();
    for (name, value) in headers {
        request_headers.insert(name, value);
    }
    let raw = mur_roost::route(method, path, &request_headers, body, state);
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

fn live_capsules(state: &Arc<State>, session_id: &str) -> u64 {
    let (status, body) = route(state, "GET", &format!("/status/{session_id}"), &[], "");
    assert_eq!(status, 200, "{body}");
    body["live_capsules"].as_u64().unwrap()
}

fn spawn_worker(state: &Arc<State>, credential: &str) -> (u16, Value) {
    route(
        state,
        "POST",
        "/spawn",
        &[(SPAWN_CREDENTIAL_HEADER, credential)],
        r#"{"name":"worker","version":"0.1.0"}"#,
    )
}

/// Two running capsules — one standalone, one a formation's member — fill two of three slots in
/// a daemon that started after them. Killing one gives its slot back without touching its record.
#[test]
fn a_restarted_daemon_counts_running_capsules_until_they_exit() {
    let server = common::ScriptedServer::start(Vec::new());
    let home = driver_home();
    let formation = FormationId::mint().to_string();
    let mut standalone = Capsule::start(home.path(), &server.endpoint, "standalone", &[]);
    let member = Capsule::start(
        home.path(),
        &server.endpoint,
        "member",
        &[("MURMUR_FORMATION_ID", &formation)],
    );
    for capsule in [&standalone, &member] {
        let record = record_for(home.path(), &capsule.session_id());
        wait_for("the capsule's running record", || record.exists());
    }

    let (inherited, report) =
        mur_roost::census::inherit(&home.path().join(".murmur").join("running")).unwrap();
    assert_eq!(report.counted, 2, "{report:?}");

    let registry = tempfile::tempdir().unwrap();
    publish(registry.path(), "root", ROOT_BODY);
    publish(registry.path(), "worker", WORKER_BODY);
    let state = Arc::new(State {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        registry_path: registry.path().to_path_buf(),
        spawn_allow: vec!["root".to_string()],
        max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
        max_concurrent: mur_roost::bounds::DEFAULT_MAX_CONCURRENT,
        max_live_capsules: 3,
        inherited: Arc::new(Mutex::new(inherited)),
        authority: Arc::new(SpawnAuthority::generate().unwrap()),
    });
    let root = "ses_0199restartedroot00000000000001";
    let (status, body) = route(
        &state,
        "POST",
        "/register",
        &[],
        &format!(r#"{{"session_id":"{root}","name":"root","version":"0.1.0"}}"#),
    );
    assert_eq!(status, 200, "{body}");
    let credential = body["credential"].as_str().unwrap().to_string();

    assert_eq!(live_capsules(&state, root), 3);
    let (status, body) = spawn_worker(&state, &credential);
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("machine capsule ceiling reached: "),
        "{body}"
    );

    let killed = standalone.pid();
    standalone.child.kill().unwrap();
    standalone.child.wait().unwrap();
    wait_for("the killed capsule's pid to be free", || {
        !pid_is_held(killed)
    });

    assert_eq!(live_capsules(&state, root), 2);
    let (status, body) = spawn_worker(&state, &credential);
    assert_eq!(status, 200, "{body}");
    assert!(record_for(home.path(), &standalone.session_id()).exists());

    drop(member);
}
