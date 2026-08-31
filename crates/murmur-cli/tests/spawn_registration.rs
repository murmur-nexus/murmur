//! Which sessions announce themselves to `mur-roost`, and what happens when they cannot.
//!
//! Registration is what replaces the daemon's own knowledge of a session it used to stage itself.
//! A session that declares `capabilities.spawn.allow` must register, because the daemon has to
//! hold that session's ceiling before it can referee anything the session asks for — so a
//! registration it cannot complete refuses the launch. A session that declares no spawn capability
//! has nothing to referee, opens no connection at all, and runs with no daemon anywhere.

#[path = "common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use assert_cmd::Command;
use mur_roost::{authority::SpawnAuthority, JobStatus, State};
use tempfile::TempDir;

/// A real `mur-roost` on a loopback port, counting every connection it accepts.
///
/// The count is what makes "zero connections of any kind" assertable: a capsule that skipped only
/// the registration but still called `/health` would be indistinguishable from one that stayed
/// silent if only the job store were inspected.
struct CountingRoost {
    url: String,
    connections: Arc<AtomicUsize>,
    state: Arc<State>,
    _registry: TempDir,
}

impl CountingRoost {
    fn start() -> Self {
        let registry = TempDir::new().unwrap();
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path: registry.path().to_path_buf(),
            spawn_allow: vec!["capsule".to_string()],
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));

        let accept_state = Arc::clone(&state);
        let accept_count = Arc::clone(&connections);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                accept_count.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&accept_state);
                thread::spawn(move || mur_roost::handle_connection(stream, state));
            }
        });

        Self {
            url,
            connections,
            state,
            _registry: registry,
        }
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn sessions(&self) -> Vec<(String, JobStatus)> {
        self.state
            .jobs
            .lock()
            .unwrap()
            .iter()
            .map(|(id, job)| (id.clone(), job.status.clone()))
            .collect()
    }

    /// Publish the project's capsule into the daemon's own registry, which is where the daemon
    /// resolves the manifest it derives a registrant's envelope from.
    fn publish(&self, name: &str, version: &str, manifest_body: &str) {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            use std::io::Write;
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("murmur.yaml", options).unwrap();
            zip.write_all(format!("name: {name}\nversion: {version}\n{manifest_body}").as_bytes())
                .unwrap();
            zip.start_file("capsule.wasm", options).unwrap();
            zip.write_all(
                &fs::read(common::fixture_path("run/components/capsule-env-echo.wasm")).unwrap(),
            )
            .unwrap();
            zip.finish().unwrap();
        }
        murmur_artifact::Registry::publish(
            &murmur_artifact::LocalRegistry::new(self.state.registry_path.clone()),
            murmur_artifact::ArtifactMeta {
                name: name.to_string(),
                version: version.to_string(),
                runtime: murmur_artifact::RuntimeType::Wasm,
                artifact_runtime: "capsule".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
            },
            &cursor.into_inner(),
        )
        .unwrap();
    }
}

/// A project directory holding one script capsule and the given capability block.
fn project(dir: &Path, capabilities: &str) -> PathBuf {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: capsule\nversion: 0.0.1\nartifacts: []\n{capabilities}"),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-env-echo.wasm"),
        dir.join("capsule.wasm"),
    )
    .unwrap();
    dir.join("murmur.yaml")
}

fn run(home: &TempDir, manifest: &Path, roost_url: Option<&str>) -> assert_cmd::assert::Assert {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("MURMUR_ROOST_URL");
    if let Some(url) = roost_url {
        command.env("MURMUR_ROOST_URL", url);
    }
    command
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--no-env-file",
        ])
        .assert()
}

const SPAWN_CAPABILITIES: &str = "capabilities:\n  spawn:\n    allow:\n      - worker\n";

/// A capsule that declares `capabilities.spawn.allow` registers exactly once, appears as running
/// while it runs, and is retired when it ends.
#[test]
fn a_capsule_declaring_spawn_allow_registers_once() {
    if capsule_runtime::skip_without_host_support("a_capsule_declaring_spawn_allow_registers_once")
    {
        return;
    }
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = project(dir.path(), SPAWN_CAPABILITIES);
    let roost = CountingRoost::start();
    roost.publish("capsule", "0.0.1", SPAWN_CAPABILITIES);

    run(&home, &manifest, Some(&roost.url)).success();

    // One registration and one deregistration: the pair a session opens and closes with, and
    // nothing else.
    assert_eq!(roost.connections(), 2, "{:?}", roost.sessions());
    let sessions = roost.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let (session_id, status) = &sessions[0];
    assert!(session_id.starts_with("ses_"), "{session_id}");
    assert_eq!(*status, JobStatus::Complete);
}

/// A capsule that declares no spawn capability opens no connection at all, even with a daemon
/// listening and `MURMUR_ROOST_URL` pointed at it.
#[test]
fn a_capsule_that_declares_no_spawn_capability_never_contacts_roost() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = project(dir.path(), "");
    let roost = CountingRoost::start();

    run(&home, &manifest, Some(&roost.url)).success();

    assert_eq!(roost.connections(), 0);
    assert!(roost.sessions().is_empty(), "{:?}", roost.sessions());
}

/// The same capsule, with nothing listening and `MURMUR_ROOST_URL` unset: no daemon dependency.
#[test]
fn a_capsule_that_declares_no_spawn_capability_runs_with_no_daemon() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = project(dir.path(), "");

    run(&home, &manifest, None).success();
}

/// A capsule that *can* delegate and cannot register is refused, rather than run outside the
/// knowledge of the daemon that bounds what it may spawn.
#[test]
fn a_declaring_capsule_refuses_to_launch_when_the_daemon_is_unreachable() {
    if capsule_runtime::skip_without_host_support(
        "a_declaring_capsule_refuses_to_launch_when_the_daemon_is_unreachable",
    ) {
        return;
    }
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = project(dir.path(), SPAWN_CAPABILITIES);

    // Port 1 is reserved and nothing listens there.
    let assert = run(&home, &manifest, Some("http://127.0.0.1:1")).failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

    assert!(stderr.contains("E-RUN-019"), "{stderr}");
    assert!(
        stderr.contains("failed to register this session with mur-roost"),
        "{stderr}"
    );
    assert!(stderr.contains("http://127.0.0.1:1"), "{stderr}");
    assert!(stderr.contains("capabilities.spawn.allow"), "{stderr}");
}

/// The same refusal when the variable naming the daemon is not set at all.
#[test]
fn a_declaring_capsule_refuses_to_launch_with_no_daemon_url() {
    if capsule_runtime::skip_without_host_support(
        "a_declaring_capsule_refuses_to_launch_with_no_daemon_url",
    ) {
        return;
    }
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = project(dir.path(), SPAWN_CAPABILITIES);

    let assert = run(&home, &manifest, None).failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

    assert!(stderr.contains("E-RUN-019"), "{stderr}");
    assert!(stderr.contains("MURMUR_ROOST_URL is not set"), "{stderr}");
}

/// `mur run --capsule NAME --capsule-version VERSION` runs an installed artifact with no project
/// directory: the path a delegated child is launched on.
#[test]
fn an_installed_capsule_runs_by_name_and_version() {
    let home = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    let store = home.path().join(".murmur").join("artifacts");
    fs::create_dir_all(&store).unwrap();
    let roost = CountingRoost::start();
    roost.publish("installed", "0.1.0", "artifacts: []\n");
    // The same bytes in the store the child resolves through.
    copy_tree(&roost.state.registry_path, &store);

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .env_remove("MURMUR_ROOST_URL")
        .args([
            "run",
            "--capsule",
            "installed",
            "--capsule-version",
            "0.1.0",
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--json",
            "--no-env-file",
        ])
        .assert()
        .success();

    // Staged from the artifact bytes into the workdir, with no murmur.yaml and no lockfile written
    // beside them.
    assert!(workdir.path().join(".murmur").is_dir());
    assert!(!workdir.path().join("murmur.yaml").exists());
    assert!(!workdir.path().join("murmur.lock").exists());
    assert_eq!(roost.connections(), 0);
}

/// `--capsule` and `--capsule-version` are required together, and conflict with an explicit
/// `--manifest`.
#[test]
fn the_installed_capsule_flags_are_required_together() {
    let home = TempDir::new().unwrap();

    for arguments in [
        vec!["run", "--capsule", "installed"],
        vec!["run", "--capsule-version", "0.1.0"],
        vec![
            "run",
            "--capsule",
            "installed",
            "--capsule-version",
            "0.1.0",
            "--manifest",
            "./murmur.yaml",
        ],
    ] {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .args(&arguments)
            .assert()
            .failure()
            .code(2);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}
