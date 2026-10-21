//! `mur trace rm`: removing one ended session of one workdir root, and nothing else.
//!
//! The launched cases drive the real `mur` binary against a real Wasmtime driver and a scripted
//! provider, so what is removed is checked against the records path `mur run --verbose` printed
//! and the session directory staging made, not against a second derivation of the layout. The
//! fabricated cases write the running records and links no real launch leaves behind.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Child,
    thread,
    time::Duration,
};

use assert_cmd::{assert::Assert, Command};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "rm-capsule";

/// A pid no process holds: above the kernel's `pid_max` ceiling.
const DEAD_PID: u32 = 0x7FFF_FFFE;

/// One Anthropic response that ends the turn.
fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// A capsule manifest declaring the driver.
fn create_manifest(project_dir: &Path, endpoint: &str) -> PathBuf {
    let manifest = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
         endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    \
         allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  \
         driver:\n    artifact: {DRIVER_NAME}\n",
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// One project, its temp home and its scripted provider, wired together.
struct Fixture {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    _server: common::ScriptedServer,
    manifest: PathBuf,
}

fn fixture(responses: Vec<String>) -> Fixture {
    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let manifest = create_manifest(project.path(), &server.endpoint);
    Fixture {
        home,
        project,
        _artifacts: artifacts,
        _server: server,
        manifest,
    }
}

impl Fixture {
    /// `mur run --verbose` with `task` inline, into the workdir root `workdir` when given and into
    /// `<project>/workdir` otherwise; the records directory it printed.
    fn run(&self, task: &str, workdir: Option<&Path>) -> PathBuf {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                task,
                "--verbose",
            ]);
        if let Some(workdir) = workdir {
            command.args(["--workdir", workdir.to_str().unwrap()]);
        }
        let assert = command.assert().success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        common::parse_records_from_stdout(&stdout)
    }

    /// A workdir root inside the project, created empty.
    fn root(&self, name: &str) -> PathBuf {
        let root = self.project.path().join(name);
        fs::create_dir(&root).unwrap();
        root
    }
}

/// `mur <args>` under `home`.
fn mur(home: &Path, args: &[&str]) -> Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home)
        .args(args)
        .assert()
}

/// `mur trace rm <session> --workdir <root> --json` under `home`, which must succeed and print
/// exactly one JSON object.
fn rm_json(home: &Path, session: &str, root: &Path) -> Value {
    let assert = mur(
        home,
        &[
            "trace",
            "rm",
            session,
            "--workdir",
            root.to_str().unwrap(),
            "--json",
        ],
    )
    .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is one JSON object ({err}): {stdout}"));
    let object = value.as_object().expect("one JSON object");
    for key in ["session_id", "records_dir", "session_dir", "running_record"] {
        assert!(object.contains_key(key), "{key} is missing: {stdout}");
    }
    value
}

/// `mur trace rm <args…> --workdir <root>` under `home`, which must fail with exit 1, `code` on
/// stderr and nothing on stdout; its stderr.
fn rm_refused(home: &Path, args: &[&str], root: &Path, code: &str) -> String {
    let mut full = vec!["trace", "rm"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--workdir", root.to_str().unwrap()]);
    let assert = mur(home, &full).code(1);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(stderr.contains(&format!("error[{code}]")), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "a failure prints nothing on stdout"
    );
    stderr
}

/// `mur trace list --workdir <root> --json`'s session ids, newest first.
fn listed_ids(home: &Path, root: &Path) -> Vec<String> {
    let assert = mur(
        home,
        &[
            "trace",
            "list",
            "--workdir",
            root.to_str().unwrap(),
            "--json",
        ],
    )
    .success();
    let listing: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_string())
        .collect()
}

fn name_of(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// Every path beneath `dir` whose file name is `name`, `dir` included.
fn named_beneath(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        if next.file_name().is_some_and(|file| file == name) {
            found.push(next.clone());
        }
        if next.is_dir() && !next.is_symlink() {
            for entry in fs::read_dir(&next).unwrap() {
                pending.push(entry.unwrap().path());
            }
        }
    }
    found
}

/// A running record for `session_id` naming `pid` started at `process_start`.
fn write_record(home: &Path, session_id: &str, pid: u32, process_start: &str) -> PathBuf {
    let dir = home.join(".murmur").join("running");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "session_id": session_id,
            "url": "127.0.0.1:1",
            "pid": pid,
            "process_start": process_start,
            "capsule_name": CAPSULE_NAME,
            "capsule_version": "0.1.0",
            "workdir": "/tmp/fabricated",
            "records_dir": "/tmp/fabricated/records",
            "outlives_launcher": true,
            "started_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

/// A `sleep 300` child, killed and reaped on drop.
struct Stray {
    child: Child,
}

impl Stray {
    fn sleeping() -> Self {
        Self {
            child: std::process::Command::new("sleep")
                .arg("300")
                .spawn()
                .expect("sleep should start"),
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn token(&self) -> String {
        // Let the kernel publish the process before its start time is read.
        thread::sleep(Duration::from_millis(200));
        capsule_runtime::running::process_start_token(self.pid())
            .expect("a freshly started process has a start time")
    }

    /// Through the child handle: a signalled `sleep` stays a zombie until it is waited on.
    fn is_alive(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("the child is waitable")
            .is_none()
    }
}

impl Drop for Stray {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── S8. Two sessions of one root, removed one at a time ──────────────────────

#[test]
fn rm_removes_exactly_one_session_of_a_root() {
    let f = fixture(vec![end_turn("one"), end_turn("two")]);
    let root = f.root("D");
    let a_records = f.run("t1", Some(&root));
    let b_records = f.run("t2", Some(&root));
    let (a, b) = (name_of(&a_records), name_of(&b_records));
    let a_session = root.join(".murmur").join(&a);
    let b_session = root.join(".murmur").join(&b);
    assert!(a_session.is_dir() && b_session.is_dir());

    let removed = rm_json(f.home.path(), &a, &root);
    println!("mur trace rm --json:\n{removed:#}");
    assert_eq!(
        removed,
        json!({
            "session_id": a,
            "records_dir": a_records.to_str().unwrap(),
            "session_dir": a_session.to_str().unwrap(),
            "running_record": null,
        })
    );
    assert!(!a_records.exists(), "A's records directory");
    assert!(!a_session.exists(), "A's session directory");
    assert!(b_records.join("trace.jsonl").is_file(), "B's records");
    assert!(b_session.is_dir(), "B's session directory");
    assert_eq!(listed_ids(f.home.path(), &root), vec![b.clone()]);

    let stderr = rm_refused(f.home.path(), &[&a], &root, "E-TRC-002");
    assert!(
        stderr.contains(&format!("session {a} not found")),
        "{stderr}"
    );

    let assert = mur(
        f.home.path(),
        &["trace", "rm", "@1", "--workdir", root.to_str().unwrap()],
    )
    .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(
        stdout,
        format!(
            "removed session directory {}\nremoved records {}\n",
            b_session.display(),
            b_records.display()
        )
    );

    for name in [&a, &b] {
        assert!(
            named_beneath(&f.home.path().join(".murmur/records"), name).is_empty(),
            "{name} is left under ~/.murmur/records"
        );
        assert!(
            named_beneath(&root.join(".murmur"), name).is_empty(),
            "{name} is left under D/.murmur"
        );
    }
    assert!(root.join(".murmur").is_dir(), "<root>/.murmur itself stays");
    assert!(
        common::records_group(f.home.path(), &root).is_dir(),
        "the records group stays"
    );
    assert!(listed_ids(f.home.path(), &root).is_empty());
}

// ── S9. A run without --workdir ──────────────────────────────────────────────

#[test]
fn rm_removes_the_session_directory_of_a_run_without_workdir() {
    let f = fixture(vec![end_turn("one")]);
    let records = f.run("t1", None);
    let id = name_of(&records);
    let root = f.project.path().join("workdir");
    let session = root.join(&id);
    assert!(session.is_dir(), "staging made {}", session.display());

    let removed = rm_json(f.home.path(), &id, &root);
    assert_eq!(removed["session_dir"], session.to_str().unwrap());
    assert_eq!(removed["records_dir"], records.to_str().unwrap());
    assert!(removed["running_record"].is_null());
    assert!(!session.exists());
    assert!(!records.exists());
    assert!(root.is_dir(), "the workdir root stays");
}

// ── S10. A running session is refused; a stale record goes with the session ──

#[test]
fn rm_refuses_a_running_session_and_prunes_a_stale_record() {
    let f = fixture(vec![end_turn("one")]);
    let root = f.root("D");
    let records = f.run("t1", Some(&root));
    let id = name_of(&records);
    let session = root.join(".murmur").join(&id);

    let mut stray = Stray::sleeping();
    let record = write_record(f.home.path(), &id, stray.pid(), &stray.token());
    let stderr = rm_refused(f.home.path(), &[&id], &root, "E-RUN-024");
    println!("refused:\n{stderr}");
    assert!(stderr.contains(&id), "{stderr}");
    assert!(stderr.contains(&format!("pid {}", stray.pid())), "{stderr}");
    assert!(stderr.contains(&format!("mur stop {id}")), "{stderr}");
    assert!(records.is_dir() && session.is_dir() && record.is_file());
    assert!(stray.is_alive(), "mur trace rm signals nothing");

    write_record(f.home.path(), &id, DEAD_PID, "1");
    let removed = rm_json(f.home.path(), &id, &root);
    assert_eq!(removed["running_record"], record.to_str().unwrap());
    assert_eq!(removed["records_dir"], records.to_str().unwrap());
    assert_eq!(removed["session_dir"], session.to_str().unwrap());
    assert!(!record.exists(), "the stale running record");
    assert!(!records.exists() && !session.exists());
}

// ── S11. What an address may name, and what is never followed ────────────────

#[test]
fn rm_refuses_paths_unknown_ids_and_links_and_finishes_a_half_removal() {
    let f = fixture(vec![end_turn("one"), end_turn("two")]);
    let root = f.root("D");
    let records = f.run("t1", Some(&root));
    let id = name_of(&records);
    let session = root.join(".murmur").join(&id);

    let literal = session.to_str().unwrap().to_string();
    let stderr = rm_refused(f.home.path(), &[&literal], &root, "E-TRC-002");
    assert!(
        stderr.contains(&format!(
            "'{literal}' is not a session address: mur trace rm takes @N, a ses_ id or a suffix \
             of one"
        )),
        "{stderr}"
    );
    rm_refused(f.home.path(), &["trace.jsonl"], &root, "E-TRC-002");
    let unknown = format!("ses_{}", "0".repeat(32));
    rm_refused(f.home.path(), &[&unknown], &root, "E-TRC-002");
    assert!(records.is_dir() && session.is_dir(), "nothing was removed");

    // `<root>/.murmur` swapped for a link to a directory holding the session's name.
    let elsewhere = f.project.path().join("elsewhere");
    fs::rename(root.join(".murmur"), &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join(".murmur")).unwrap();
    let stderr = rm_refused(f.home.path(), &[&id], &root, "E-RUN-050");
    println!("refused:\n{stderr}");
    assert!(stderr.contains("hint:"), "{stderr}");
    assert!(
        elsewhere.join(&id).is_dir(),
        "the link's target is untouched"
    );
    assert!(records.is_dir(), "nothing was removed");
    fs::remove_file(root.join(".murmur")).unwrap();
    fs::rename(&elsewhere, root.join(".murmur")).unwrap();

    // A full id whose records directory an earlier removal already took.
    fs::remove_dir_all(&records).unwrap();
    let removed = rm_json(f.home.path(), &id, &root);
    assert!(removed["records_dir"].is_null(), "{removed:#}");
    assert_eq!(removed["session_dir"], session.to_str().unwrap());
    assert!(!session.exists());

    // A suffix names a session through the records group alone.
    let second = f.run("t2", Some(&root));
    let suffix = &name_of(&second)[28..];
    let removed = rm_json(f.home.path(), suffix, &root);
    assert_eq!(removed["session_id"], name_of(&second).as_str());
    assert!(!second.exists());
}
