//! `mur trace list`: the sessions of one workdir root, as `mur` itself addresses them.
//!
//! The launched cases drive the real `mur` binary against a real Wasmtime driver and a scripted
//! provider, so the listing is checked against the records paths `mur run --verbose` printed
//! rather than against a second derivation of the records layout. The fabricated cases write the
//! session directories and running records no real launch leaves behind.

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
const CAPSULE_NAME: &str = "list-capsule";

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
    /// `mur run --verbose` with `task` inline into the workdir root `workdir`; the records
    /// directory it printed.
    fn run(&self, task: &str, workdir: &Path) -> PathBuf {
        let assert = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                task,
                "--workdir",
                workdir.to_str().unwrap(),
                "--verbose",
            ])
            .assert()
            .success();
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

/// `mur trace list --workdir <root> --json` under `home`, which must succeed and print exactly
/// one JSON object.
fn list_json(home: &Path, root: &Path) -> Value {
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
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("stdout is one JSON object ({err}): {stdout}"));
    assert!(value.is_object(), "{stdout}");
    value
}

/// `mur trace list --workdir <root>` under `home`, which must succeed; its stdout.
fn list_table(home: &Path, root: &Path) -> String {
    let assert = mur(
        home,
        &["trace", "list", "--workdir", root.to_str().unwrap()],
    )
    .success();
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn sessions(listing: &Value) -> &Vec<Value> {
    listing["sessions"]
        .as_array()
        .expect("sessions is an array")
}

fn ids(listing: &Value) -> Vec<String> {
    sessions(listing)
        .iter()
        .map(|session| session["session_id"].as_str().unwrap().to_string())
        .collect()
}

fn name_of(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// The `context_id` of the first `task_start` in the trace a records directory holds.
fn task_context_id(records_dir: &Path) -> String {
    fs::read_to_string(records_dir.join("trace.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "task_start")
        .expect("a run that took a task records its task_start")["context_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The mint time a `ses_` id carries, as the listing formats it.
fn minted_at(session_id: &str) -> String {
    let ms = i64::from_str_radix(&session_id["ses_".len().."ses_".len() + 12], 16).unwrap();
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The minimum `session_start` `mur`'s own reader accepts.
fn session_start_line(session_id: &str) -> String {
    json!({
        "event_type": "session_start",
        "session_id": session_id,
        "capsule_name": CAPSULE_NAME,
        "capsule_version": "0.1.0",
        "model": "test-model",
        "max_turns": 8
    })
    .to_string()
}

/// A hand-written session records directory under `group`, holding `trace` verbatim as its
/// `trace.jsonl`, or no trace at all for `None`.
fn write_session(group: &Path, session_id: &str, trace: Option<&str>) {
    let dir = group.join(session_id);
    fs::create_dir_all(&dir).unwrap();
    if let Some(trace) = trace {
        fs::write(dir.join("trace.jsonl"), trace).unwrap();
    }
}

fn running_dir(home: &Path) -> PathBuf {
    home.join(".murmur").join("running")
}

/// A running record for `session_id` naming `pid` started at `process_start`.
fn write_record(home: &Path, session_id: &str, pid: u32, process_start: &str) -> PathBuf {
    let dir = running_dir(home);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.json"));
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "session_id": session_id,
            "url": "127.0.0.1:1",
            "pid": pid,
            "process_start": process_start,
            "capsule_name": "fabricated",
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

// ── S1. Each workdir root lists exactly its own sessions ─────────────────────

#[test]
fn two_workdirs_list_exactly_their_own_sessions() {
    let f = fixture(vec![end_turn("one"), end_turn("two"), end_turn("three")]);
    let (a, b) = (f.root("A"), f.root("B"));
    let a1 = f.run("t1", &a);
    let a2 = f.run("t2", &a);
    let b1 = f.run("t3", &b);

    for (root, expected) in [(&a, vec![&a2, &a1]), (&b, vec![&b1])] {
        let listing = list_json(f.home.path(), root);
        let group = common::records_group(f.home.path(), root);
        assert_eq!(
            listing["workdir_root"],
            root.to_str().unwrap(),
            "{listing:#}"
        );
        assert_eq!(
            listing["records_group"],
            group.to_str().unwrap(),
            "{listing:#}"
        );
        let expected_ids: Vec<String> = expected.iter().map(|dir| name_of(dir)).collect();
        assert_eq!(ids(&listing), expected_ids, "newest first: {listing:#}");

        for (session, records) in sessions(&listing).iter().zip(&expected) {
            let id = session["session_id"].as_str().unwrap();
            assert_eq!(session["records_dir"], records.to_str().unwrap());
            assert_eq!(Path::new(records), group.join(id));
            assert_eq!(session["context_id"], task_context_id(records).as_str());
            assert_eq!(session["running"], false);
            let started = session["started_at"].as_str().unwrap();
            chrono::DateTime::parse_from_rfc3339(started)
                .unwrap_or_else(|err| panic!("started_at {started} is RFC 3339: {err}"));
            assert_eq!(started, minted_at(id));
        }
    }

    let a_ids = ids(&list_json(f.home.path(), &a));
    assert!(!a_ids.contains(&name_of(&b1)), "{a_ids:?}");
    let b_ids = ids(&list_json(f.home.path(), &b));
    assert!(!b_ids.contains(&name_of(&a1)) && !b_ids.contains(&name_of(&a2)));
}

// ── S2. The table, and the ordinals it lines up with ─────────────────────────

#[test]
fn the_table_lists_the_sessions_the_ordinals_name() {
    let f = fixture(vec![end_turn("one"), end_turn("two")]);
    let a = f.root("A");
    f.run("t1", &a);
    f.run("t2", &a);
    let listing = list_json(f.home.path(), &a);
    let expected = ids(&listing);
    assert_eq!(expected.len(), 2);

    let table = list_table(f.home.path(), &a);
    let mut lines = table.lines();
    let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        ["SESSION", "STARTED", "STATUS", "CONTEXT"],
        "{table}"
    );
    let rows: Vec<Vec<&str>> = lines
        .by_ref()
        .take(expected.len())
        .map(|line| line.split_whitespace().collect())
        .collect();
    for (row, session) in rows.iter().zip(sessions(&listing)) {
        assert_eq!(row[0], session["session_id"].as_str().unwrap(), "{table}");
        assert_eq!(row[1], session["started_at"].as_str().unwrap(), "{table}");
        assert_eq!(row[2], "ended", "{table}");
        assert_eq!(row[3], session["context_id"].as_str().unwrap(), "{table}");
    }
    assert_eq!(
        lines.next(),
        Some(format!("records: {}", listing["records_group"].as_str().unwrap()).as_str()),
        "{table}"
    );

    for (i, id) in expected.iter().enumerate() {
        let ordinal = format!("@{}", i + 1);
        let show = mur(
            f.home.path(),
            &["trace", "show", &ordinal, "--workdir", a.to_str().unwrap()],
        )
        .success();
        let stdout = String::from_utf8_lossy(&show.get_output().stdout).to_string();
        assert!(
            stdout.contains(&format!("session:    {id}")),
            "{ordinal} names entry {i}: {stdout}"
        );
    }
}

// ── S3. A session with no task to read is listed with no context ─────────────

#[test]
fn sessions_with_no_readable_task_list_with_a_null_context() {
    let home = tempfile::tempdir().unwrap();
    let w = home.path().join("W");
    let group = common::records_group(home.path(), &w);
    let no_task = "ses_019f0193c7d871a5b2e30ff41a7c0ce1";
    let no_trace = "ses_019f0193c8a071a5b2e30ff41a7c0ce2";
    let torn = "ses_019f0193c96871a5b2e30ff41a7c0ce3";
    write_session(
        &group,
        no_task,
        Some(&format!("{}\n", session_start_line(no_task))),
    );
    write_session(&group, no_trace, None);
    write_session(
        &group,
        torn,
        Some(&format!(
            "{}\n{{\"event_type\":\"task_start\",\"session_id\":\"{torn}\",\"conte",
            session_start_line(torn)
        )),
    );

    let listing = list_json(home.path(), &w);
    assert_eq!(ids(&listing), [torn, no_trace, no_task], "{listing:#}");
    for session in sessions(&listing) {
        let id = session["session_id"].as_str().unwrap();
        assert_eq!(session["context_id"], Value::Null, "{listing:#}");
        assert_eq!(session["started_at"], minted_at(id).as_str());
        assert_eq!(session["records_dir"], group.join(id).to_str().unwrap());
        assert_eq!(session["running"], false);
    }

    let table = list_table(home.path(), &w);
    assert!(
        table.contains(&format!("{no_trace}  {}  ended    -", minted_at(no_trace))),
        "{table}"
    );
}

// ── S4. An empty group, and a group whose workdir is gone ────────────────────

#[test]
fn a_never_run_workdir_lists_no_sessions_and_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let w = home.path().join("never-run");
    let group = common::records_group(home.path(), &w);

    let listing = list_json(home.path(), &w);
    assert_eq!(listing["sessions"], json!([]), "{listing:#}");
    assert_eq!(listing["records_group"], group.to_str().unwrap());

    let table = list_table(home.path(), &w);
    assert_eq!(
        table,
        format!(
            "no sessions in workdir at {} (records in {})\n",
            w.display(),
            group.display()
        )
    );
    let left: Vec<_> = fs::read_dir(home.path()).unwrap().collect();
    assert!(left.is_empty(), "the listing creates nothing: {left:?}");
}

#[test]
fn a_deleted_workdir_still_lists_its_session() {
    let f = fixture(vec![end_turn("one")]);
    let a = f.root("A");
    let records = f.run("t1", &a);
    fs::remove_dir_all(&a).unwrap();

    let listing = list_json(f.home.path(), &a);
    assert_eq!(ids(&listing), [name_of(&records)], "{listing:#}");
    assert_eq!(
        sessions(&listing)[0]["records_dir"],
        records.to_str().unwrap()
    );
}

// ── S5. Running is read from the records, and nothing is pruned ──────────────

#[test]
fn running_is_judged_by_the_process_a_record_names_and_nothing_is_pruned() {
    let home = tempfile::tempdir().unwrap();
    let w = home.path().join("W");
    let group = common::records_group(home.path(), &w);
    let live = "ses_019f0193c7d871a5b2e30ff41a7c0ce1";
    let dead = "ses_019f0193c8a071a5b2e30ff41a7c0ce2";
    write_session(
        &group,
        live,
        Some(&format!("{}\n", session_start_line(live))),
    );
    write_session(
        &group,
        dead,
        Some(&format!("{}\n", session_start_line(dead))),
    );
    let mut stray = Stray::sleeping();
    let live_record = write_record(home.path(), live, stray.pid(), &stray.token());
    let dead_record = write_record(home.path(), dead, DEAD_PID, "0");

    let listing = list_json(home.path(), &w);
    let running: Vec<(String, bool)> = sessions(&listing)
        .iter()
        .map(|session| {
            (
                session["session_id"].as_str().unwrap().to_string(),
                session["running"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        running,
        [(dead.to_string(), false), (live.to_string(), true)],
        "{listing:#}"
    );
    let table = list_table(home.path(), &w);
    assert!(
        table.contains(&format!("{live}  {}  running", minted_at(live))),
        "{table}"
    );

    assert!(live_record.exists(), "the live record is kept");
    assert!(dead_record.exists(), "the dead record is not pruned");
    assert!(stray.is_alive(), "the listing signals nothing");
}

// ── S6. A running directory that cannot be listed fails the listing ──────────

#[test]
fn an_unlistable_running_directory_is_e_run_028_with_nothing_on_stdout() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join(".murmur")).unwrap();
    fs::write(running_dir(home.path()), b"not a directory").unwrap();
    let w = home.path().join("W");
    let session = "ses_019f0193c7d871a5b2e30ff41a7c0ce1";
    write_session(
        &common::records_group(home.path(), &w),
        session,
        Some(&format!("{}\n", session_start_line(session))),
    );

    for json in [true, false] {
        let mut args = vec!["trace", "list", "--workdir", w.to_str().unwrap()];
        if json {
            args.push("--json");
        }
        let failure = mur(home.path(), &args).code(1);
        let output = failure.get_output();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("error[E-RUN-028]"), "{stderr}");
        assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    }
}
