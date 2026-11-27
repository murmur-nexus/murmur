//! Retention end to end: what a `retain:` block deletes, what the absence of one deletes, and
//! what `mur conversation` shows an operator about the store that is left.
//!
//! Every case drives the real `mur` binary against a real Wasmtime driver and a real filesystem,
//! because the properties under test are all about what is on disk after a launch: which session
//! directories survive, which lines a record still holds, and whether the trace says why. The
//! session fixtures are hand-built `ses_<32 hex>` directories, which is what makes "the decision
//! came from the id, not from a `stat`" observable at all.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::{assert::Assert, Command};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "retention-capsule";
const TASK_TEXT: &str = "Say something short.";
const CONTEXT_ID: &str = "ctx_fixed";

// ── Fixture ──────────────────────────────────────────────────────────────────

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

fn responses(n: usize) -> Vec<String> {
    (0..n).map(|i| end_turn(&format!("reply {i}"))).collect()
}

struct Fixture {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    _server: common::ScriptedServer,
    manifest: PathBuf,
    /// `--workdir` for every run: the work folder every run shares, whose sessions' records are in
    /// its records group.
    workdir: TempDir,
}

impl Fixture {
    fn new(responses: Vec<String>, blocks: &str) -> Self {
        Self::with_delay(responses, blocks, Duration::ZERO)
    }

    fn with_delay(responses: Vec<String>, blocks: &str, delay: Duration) -> Self {
        let server = common::ScriptedServer::start_with_delay(responses, delay);
        let home = tempfile::tempdir().unwrap();
        let artifacts = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();

        let driver = common::create_driver_artifact(
            artifacts.path(),
            DRIVER_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&home, &driver).success();

        let manifest = write_manifest(project.path(), CAPSULE_NAME, &server.endpoint, blocks);
        Self {
            home,
            project,
            _artifacts: artifacts,
            _server: server,
            manifest,
            workdir,
        }
    }

    /// The records group holding every `ses_*` records directory of this fixture's runs.
    fn sessions_root(&self) -> PathBuf {
        common::records_group(self.home.path(), self.workdir.path())
    }

    /// The entries of the shared work folder, which no run of this fixture writes into.
    fn work_folder_entries(&self) -> Vec<String> {
        fs::read_dir(self.workdir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// The records directory of a session whose uuid-v7 timestamp is `ms`.
    fn plant(&self, ms: u64, tail: u64) -> String {
        plant_session(&self.sessions_root(), ms, tail)
    }

    fn run(&self, extra: &[&str]) -> Assert {
        run_manifest(&self.home, &self.manifest, self.workdir.path(), extra)
    }

    fn record_path(&self, context_id: &str) -> PathBuf {
        record_path_in(&self.home, CAPSULE_NAME, context_id)
    }
}

fn write_manifest(dir: &Path, name: &str, endpoint: &str, blocks: &str) -> PathBuf {
    let manifest = format!(
        "name: {name}\nversion: 0.1.0\n{blocks}artifacts:\n  - name: {DRIVER_NAME}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
         endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    \
         allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  \
         driver:\n    artifact: {DRIVER_NAME}\n",
    );
    let path = dir.join(format!("{name}.yaml"));
    fs::write(&path, manifest).unwrap();
    path
}

fn run_manifest(home: &TempDir, manifest: &Path, workdir: &Path, extra: &[&str]) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path());
    cmd.args([
        "run",
        "--manifest",
        manifest.to_str().unwrap(),
        "--task",
        TASK_TEXT,
        "--workdir",
        workdir.to_str().unwrap(),
    ]);
    cmd.args(extra);
    cmd.assert()
}

fn conversation(home: &TempDir, args: &[&str]) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path());
    cmd.arg("conversation").args(args).assert()
}

fn record_path_in(home: &TempDir, record: &str, context_id: &str) -> PathBuf {
    home.path()
        .join(".murmur/conversations")
        .join(record)
        .join(context_id)
        .join("conversation.jsonl")
}

/// Every `ses_*` directory under a sessions root, sorted — which for uuid-v7 names is oldest
/// first.
fn sessions(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("ses_"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A `ses_` directory whose uuid-v7 timestamp is `ms`, created now — so its own mtime is fresh
/// however old its id claims to be.
fn plant_session(root: &Path, ms: u64, tail: u64) -> String {
    let name = format!("ses_{ms:012x}{tail:020x}");
    let dir = root.join(&name);
    fs::create_dir_all(dir.join("blobs")).unwrap();
    fs::write(dir.join("trace.jsonl"), "{}\n").unwrap();
    fs::write(dir.join("blobs").join("deadbeef"), b"body").unwrap();
    name
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Every line of one record, non-empty lines only.
fn record_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The message ids one record holds, in file order. A line with no string `role` is not a
/// message and is skipped, exactly as every runtime reader skips it.
fn record_message_ids(path: &Path) -> Vec<String> {
    record_lines(path)
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("role").and_then(Value::as_str).is_some())
        .map(|value| value["id"].as_str().unwrap().to_string())
        .collect()
}

/// Every `retention` event in one session's trace.
fn retention_events(session_dir: &Path) -> Vec<Value> {
    fs::read_to_string(session_dir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("event_type").and_then(Value::as_str) == Some("retention"))
        .collect()
}

fn stdout_of(assert: Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

// ── Absent means unlimited ─────────────────────────────────────────────────

/// The invariant the whole slice is judged against: a capsule with a `trace:` block carrying only
/// `capture` and a `context:` block carrying only `record` deletes nothing, writes no header line
/// and writes no `retention` event, however many times it runs. Upgrading changes no existing
/// capsule's behaviour.
#[test]
fn no_retain_block_anywhere_deletes_nothing() {
    let f = Fixture::new(
        responses(4),
        "trace:\n  capture: content\ncontext:\n  record: on\n",
    );

    let mut expected_lines = 0;
    for _ in 0..4 {
        f.run(&["--context", CONTEXT_ID]).success();
        expected_lines += 2; // the task's user message and the assistant reply
        assert_eq!(
            record_lines(&f.record_path(CONTEXT_ID)).len(),
            expected_lines,
            "no line is ever removed and none is ever added beside the messages"
        );
    }

    let names = sessions(&f.sessions_root());
    assert_eq!(
        names.len(),
        4,
        "every session directory is still there: {names:?}"
    );

    for line in record_lines(&f.record_path(CONTEXT_ID)) {
        let value: Value = serde_json::from_str(&line).unwrap();
        assert!(
            value.get("role").and_then(Value::as_str).is_some(),
            "no header line is written for a capsule with no retention policy: {line}"
        );
    }

    for name in &names {
        assert!(
            retention_events(&f.sessions_root().join(name)).is_empty(),
            "{name} recorded a retention event with no policy declared"
        );
    }
    drop(f.project);
}

// ── Max_sessions ───────────────────────────────────────────────────────────

/// Six runs with `max_sessions: 3` leave the three lexically greatest `ses_` ids, the sixth run's
/// own among them, and each removed directory goes whole — no orphaned `trace.jsonl`, no orphaned
/// `blobs/`. The deletion is recorded in the trace of the run that performed it.
#[test]
fn max_sessions_leaves_exactly_the_newest_three_and_says_so_in_the_trace() {
    let f = Fixture::new(
        responses(6),
        "trace:\n  capture: meta\n  retain:\n    max_sessions: 3\n",
    );

    let mut seen: Vec<String> = Vec::new();
    for _ in 0..6 {
        f.run(&[]).success();
        let names = sessions(&f.sessions_root());
        for name in &names {
            if !seen.contains(name) {
                seen.push(name.clone());
            }
        }
        assert!(names.len() <= 3, "never more than three survive: {names:?}");
    }
    assert_eq!(seen.len(), 6, "six distinct sessions ran: {seen:?}");

    let survivors = sessions(&f.sessions_root());
    let mut newest_three = seen.clone();
    newest_three.sort();
    assert_eq!(survivors, newest_three[3..].to_vec());

    for gone in &newest_three[..3] {
        let dir = f.sessions_root().join(gone);
        assert!(!dir.exists(), "{gone} must go whole");
        assert!(!dir.join("trace.jsonl").exists());
        assert!(!dir.join("blobs").exists());
    }
    assert_eq!(
        f.work_folder_entries(),
        Vec::<String>::new(),
        "six runs under --workdir leave nothing in the work folder"
    );

    // the newest session's own trace names what it removed and why.
    let events = retention_events(&f.sessions_root().join(survivors.last().unwrap()));
    assert_eq!(
        events.len(),
        1,
        "one event for the one (store, reason) pair: {events:?}"
    );
    let event = &events[0];
    assert_eq!(event["store"], "sessions");
    assert_eq!(event["reason"], "max_sessions");
    assert_eq!(event["removed"], 1);
    assert_eq!(event["targets"], json!([newest_three[2]]));
    assert!(
        event["messages_dropped"].is_null(),
        "messages_dropped is written for max_messages only: {event}"
    );
    assert!(event["parent_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("evt_")));

    // …and `mur trace show` renders it where the operator already looks.
    let mut cmd = Command::cargo_bin("mur").unwrap();
    let shown = stdout_of(
        cmd.env("HOME", f.home.path())
            .args([
                "trace",
                "show",
                survivors.last().unwrap(),
                "--workdir",
                f.workdir.path().to_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert!(shown.contains("Retention"), "{shown}");
    assert!(
        shown.contains("sessions  max_sessions  removed 1"),
        "{shown}"
    );
    assert!(shown.contains(&newest_three[2]), "{shown}");
    drop(f.project);
}

// ── Max_age reads the id ───────────────────────────────────────────────────

/// The two directories deleted here have the *freshest* mtimes in the fixture — they are created
/// last, moments before the run — and their ids encode two hours ago. Deleting them anyway is
/// proof the decision came from the id and not from the filesystem.
#[test]
fn max_age_prunes_by_the_session_id_and_never_by_the_mtime() {
    let f = Fixture::new(
        responses(1),
        "trace:\n  capture: meta\n  retain:\n    max_age: 15m\n",
    );
    fs::create_dir_all(f.sessions_root()).unwrap();
    let now = now_ms();
    let recent = f.plant(now - 60_000, 3);
    let old_a = f.plant(now - 7_200_000, 1);
    let old_b = f.plant(now - 7_100_000, 2);

    f.run(&[]).success();

    let survivors = sessions(&f.sessions_root());
    assert!(!survivors.contains(&old_a), "{survivors:?}");
    assert!(!survivors.contains(&old_b), "{survivors:?}");
    assert!(survivors.contains(&recent), "{survivors:?}");
    assert_eq!(
        survivors.len(),
        2,
        "the recent fixture and this run: {survivors:?}"
    );

    let this_run = survivors.iter().find(|name| *name != &recent).unwrap();
    let events = retention_events(&f.sessions_root().join(this_run));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], "max_age");
    assert_eq!(events[0]["removed"], 2);
    drop(f.project);
}

// ── Both keys ANDed ────────────────────────────────────────────────────────

/// A survivor has to be inside both limits: an old-id directory never survives on rank, and a
/// recent-id directory outside the count does not survive on age.
#[test]
fn both_trace_keys_keep_only_sessions_inside_both() {
    let f = Fixture::new(
        responses(1),
        "trace:\n  capture: meta\n  retain:\n    max_sessions: 2\n    max_age: 15m\n",
    );
    fs::create_dir_all(f.sessions_root()).unwrap();
    let now = now_ms();
    let old = [f.plant(now - 7_200_000, 1), f.plant(now - 7_100_000, 2)];
    let recent = [
        f.plant(now - 300_000, 3),
        f.plant(now - 200_000, 4),
        f.plant(now - 100_000, 5),
    ];

    f.run(&[]).success();

    let survivors = sessions(&f.sessions_root());
    assert_eq!(survivors.len(), 2, "{survivors:?}");
    assert!(
        survivors.contains(&recent[2]),
        "the newest fixture is inside both limits: {survivors:?}"
    );
    for name in old.iter().chain(&recent[..2]) {
        assert!(!survivors.contains(name), "{name} survived: {survivors:?}");
    }

    let this_run = survivors.iter().find(|name| *name != &recent[2]).unwrap();
    let events = retention_events(&f.sessions_root().join(this_run));
    let mut by_reason: Vec<(String, u64)> = events
        .iter()
        .map(|event| {
            (
                event["reason"].as_str().unwrap().to_string(),
                event["removed"].as_u64().unwrap(),
            )
        })
        .collect();
    by_reason.sort();
    assert_eq!(
        by_reason,
        vec![("max_age".to_string(), 2), ("max_sessions".to_string(), 2)],
        "one event per (store, reason) pair that removed anything: {events:?}"
    );
    drop(f.project);
}

// ── The current session is never a candidate ───────────────────────────────

/// `max_sessions: 1` and `max_age: 1s` against a run that takes longer than a second. The
/// running session's own directory and `trace.jsonl` are still there when it exits — the hard
/// floor refuses to consider any id at or after the current session's own.
#[test]
fn the_running_session_survives_a_policy_that_would_otherwise_delete_it() {
    let f = Fixture::with_delay(
        responses(1),
        "trace:\n  capture: meta\n  retain:\n    max_sessions: 1\n    max_age: 1s\n",
        Duration::from_millis(1500),
    );
    fs::create_dir_all(f.sessions_root()).unwrap();
    let older = f.plant(now_ms() - 5_000, 1);

    let started = std::time::Instant::now();
    f.run(&[]).success();
    assert!(
        started.elapsed() > Duration::from_secs(1),
        "the run has to outlast the age window for this to prove anything"
    );

    let survivors = sessions(&f.sessions_root());
    assert_eq!(survivors.len(), 1, "{survivors:?}");
    assert_ne!(survivors[0], older);
    assert!(f
        .sessions_root()
        .join(&survivors[0])
        .join("trace.jsonl")
        .exists());
    drop(f.project);
}

// ── Max_messages ───────────────────────────────────────────────────────────

/// `max_messages` truncates the record this launch opens. The first run adopts the record with a
/// header; from the second run on, the policy applies — leaving the newest N messages with the
/// ids they have always carried, and recording the drop in both the header and the trace.
#[test]
fn max_messages_truncates_the_front_and_keeps_every_surviving_id() {
    let f = Fixture::new(
        responses(4),
        "context:\n  record: on\n  retain:\n    max_messages: 3\n",
    );
    let path = f.record_path(CONTEXT_ID);

    for _ in 0..3 {
        f.run(&["--context", CONTEXT_ID]).success();
    }
    let before = record_message_ids(&path);
    f.run(&["--context", CONTEXT_ID]).success();
    let after = record_message_ids(&path);

    assert_eq!(
        after.len(),
        3 + 2,
        "three kept at the launch's truncation, plus this run's two messages: {after:?}"
    );
    assert_eq!(
        &after[..3],
        &before[before.len() - 3..],
        "the newest three survived with the exact ids they carried"
    );

    let dropped_now = (before.len() - 3) as u64;
    let lines = record_lines(&path);
    let header: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(header["type"], "murmur.record");
    assert_eq!(header["capsule"], CAPSULE_NAME);
    // Cumulative over the record's life: run three dropped one message and run four dropped two.
    assert_eq!(header["truncated"]["dropped"], 3);
    assert_eq!(
        header["truncated"]["oldest_surviving_id"],
        before[dropped_now as usize]
    );
    assert_eq!(
        header["truncated"]["last_dropped_id"],
        before[dropped_now as usize - 1]
    );
    for line in &lines[1..] {
        let value: Value = serde_json::from_str(line).unwrap();
        assert!(
            value.get("role").and_then(Value::as_str).is_some(),
            "{line}"
        );
    }

    let newest = sessions(&f.sessions_root()).pop().unwrap();
    let events = retention_events(&f.sessions_root().join(&newest));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["store"], "records");
    assert_eq!(events[0]["reason"], "max_messages");
    assert_eq!(events[0]["removed"], 1);
    assert_eq!(events[0]["targets"], json!([CONTEXT_ID]));
    assert_eq!(
        events[0]["messages_dropped"], dropped_now,
        "the event reports what this launch dropped, not the record's lifetime total"
    );

    // a dropped id reads as truncated, not as missing.
    let dropped = &before[0];
    let out = stdout_of(conversation(&f.home, &["ls", "--message", dropped]).success());
    assert!(out.starts_with("truncated:"), "{out}");
    assert!(out.contains(dropped), "{out}");
    assert!(
        out.contains(&before[dropped_now as usize]),
        "the oldest surviving id: {out}"
    );

    // …and a surviving id reads as present.
    let survivor = &after[0];
    let out = stdout_of(conversation(&f.home, &["ls", "--message", survivor]).success());
    assert!(out.starts_with("present:"), "{out}");

    // …and an id nothing ever minted reads as unknown.
    let out = stdout_of(
        conversation(
            &f.home,
            &["ls", "--message", "msg_ffffffffffffffffffffffffffffffff"],
        )
        .success(),
    );
    assert!(out.starts_with("unknown:"), "{out}");
    drop(f.project);
}

// ── Context.retain.max_age ─────────────────────────────────────────────────

/// Age for a record is its last write. A context this capsule owns and has not written to inside
/// the window is removed whole; the context the launch is using is never removed.
#[test]
fn context_retain_max_age_drops_a_record_untouched_for_the_window() {
    let f = Fixture::new(
        responses(2),
        "context:\n  record: on\n  retain:\n    max_age: 1s\n",
    );

    f.run(&["--context", "ctx_abandoned"]).success();
    assert!(f.record_path("ctx_abandoned").exists());
    std::thread::sleep(Duration::from_millis(1500));

    f.run(&["--context", "ctx_live"]).success();

    assert!(
        !f.record_path("ctx_abandoned").exists(),
        "an over-age context directory is removed whole"
    );
    assert!(
        f.record_path("ctx_live").exists(),
        "the context this launch is using is never removed"
    );

    let newest = sessions(&f.sessions_root()).pop().unwrap();
    let events = retention_events(&f.sessions_root().join(&newest));
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["store"], "records");
    assert_eq!(events[0]["reason"], "max_age");
    assert_eq!(events[0]["targets"], json!(["ctx_abandoned"]));
    drop(f.project);
}

// ── Ownership ──────────────────────────────────────────────────────────────

/// Two capsules pointed at one `record_store`, one with a policy. The one without a policy never
/// has its history pruned by the other, because automatic pruning only touches a record whose
/// header names the capsule doing the pruning — and a capsule with no policy writes no header at
/// all, which leaves its record unowned and out of reach.
#[test]
fn two_capsules_sharing_a_record_store_prune_only_their_own() {
    let f = Fixture::new(responses(3), "");
    let store = "shared-store";
    let policed = write_manifest(
        f.project.path(),
        "policed",
        &f._server.endpoint,
        &format!("context:\n  record_store: {store}\n  retain:\n    max_age: 1s\n"),
    );
    let quiet = write_manifest(
        f.project.path(),
        "quiet",
        &f._server.endpoint,
        &format!("context:\n  record_store: {store}\n"),
    );

    run_manifest(
        &f.home,
        &quiet,
        f.workdir.path(),
        &["--context", "ctx_quiet"],
    )
    .success();
    run_manifest(
        &f.home,
        &policed,
        f.workdir.path(),
        &["--context", "ctx_policed"],
    )
    .success();
    std::thread::sleep(Duration::from_millis(1500));
    run_manifest(
        &f.home,
        &policed,
        f.workdir.path(),
        &["--context", "ctx_live"],
    )
    .success();

    assert!(
        record_path_in(&f.home, store, "ctx_quiet").exists(),
        "a record the pruning capsule does not own is never touched"
    );
    assert!(
        !record_path_in(&f.home, store, "ctx_policed").exists(),
        "its own over-age record goes"
    );
    drop(f.project);
}

/// A record carrying no header is unowned, so automatic pruning skips it however old it is. The
/// capsule that owns it adopts it on the launch that opens it — every pre-existing line surviving
/// byte for byte — and `max_messages` applies to it from that same launch.
#[test]
fn an_unowned_record_is_skipped_until_it_is_adopted() {
    let f = Fixture::new(
        responses(2),
        "context:\n  record: on\n  retain:\n    max_age: 1s\n    max_messages: 2\n",
    );

    // A record with no header: message lines only.
    let abandoned = f.record_path("ctx_unowned");
    fs::create_dir_all(abandoned.parent().unwrap()).unwrap();
    let planted: Vec<String> = (1..=3)
        .map(|n| {
            json!({"role": "user", "content": [{"type": "text", "text": format!("old {n}")}],
                   "id": format!("msg_{n:032}")})
            .to_string()
        })
        .collect();
    let headerless = format!("{}\n", planted.join("\n"));
    fs::write(&abandoned, &headerless).unwrap();
    std::thread::sleep(Duration::from_millis(1500));

    f.run(&["--context", CONTEXT_ID]).success();

    assert!(
        abandoned.exists(),
        "an unowned record is never pruned automatically, however old"
    );
    assert_eq!(
        fs::read_to_string(&abandoned).unwrap(),
        headerless,
        "a record no launch opened is untouched, and still headerless"
    );

    // The record this launch wrote is the one it adopted.
    let mine = f.record_path(CONTEXT_ID);
    let header: Value = serde_json::from_str(&record_lines(&mine)[0]).unwrap();
    assert_eq!(header["capsule"], CAPSULE_NAME);

    // Opened by the capsule that owns it: adopted, and truncated to `max_messages` in the same
    // rewrite, with this launch's own two messages appended after.
    f.run(&["--context", "ctx_unowned"]).success();

    let lines = record_lines(&abandoned);
    let header: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(header["capsule"], CAPSULE_NAME);
    assert_eq!(header["truncated"]["dropped"], 1);
    let ids = record_message_ids(&abandoned);
    assert_eq!(ids.len(), 4, "two kept plus this launch's two: {ids:?}");
    assert_eq!(
        &ids[..2],
        &[format!("msg_{:032}", 2), format!("msg_{:032}", 3)],
        "the newest two planted messages survived with their own ids"
    );

    // `mur conversation rm` is what reaches a record whose capsule has been retired.
    conversation(&f.home, &["rm", "ctx_unowned"]).success();
    assert!(!abandoned.exists());
    drop(f.project);
}

// ── The commands ───────────────────────────────────────────────────────────

/// `ls` lists every record and context with its counts, and `--json` prints the same values.
#[test]
fn conversation_ls_reports_counts_size_and_last_touched() {
    let f = Fixture::new(responses(2), "");
    f.run(&["--context", "ctx_one"]).success();
    f.run(&["--context", "ctx_two"]).success();

    let out = stdout_of(conversation(&f.home, &["ls"]).success());
    assert!(out.contains("RECORD"), "{out}");
    assert!(out.contains(CAPSULE_NAME), "{out}");
    assert!(out.contains("ctx_one") && out.contains("ctx_two"), "{out}");

    let rows: Value = serde_json::from_str(&stdout_of(
        conversation(&f.home, &["ls", "--json"]).success(),
    ))
    .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in rows {
        assert_eq!(row["record"], CAPSULE_NAME);
        assert_eq!(row["messages"], 2);
        assert!(row["bytes"].as_u64().unwrap() > 0);
        assert!(row["last_touched_ms"].as_u64().unwrap() > 0);
        assert!(
            row["truncated"].is_null(),
            "nothing has been dropped: {row}"
        );
        // No policy is declared, so nothing claimed this record.
        assert!(row["capsule"].is_null(), "{row}");
    }

    // `--record` narrows to one store.
    let rows: Value = serde_json::from_str(&stdout_of(
        conversation(&f.home, &["ls", "--record", "nothing-here", "--json"]).success(),
    ))
    .unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0);
    drop(f.project);
}

/// `truncate --keep N` leaves exactly N messages plus a header recording the drop, and reports
/// what it dropped. `rm` removes the context directory whole and reports what it held.
#[test]
fn conversation_truncate_and_rm_do_what_they_say() {
    let f = Fixture::new(responses(2), "");
    f.run(&["--context", CONTEXT_ID]).success();
    f.run(&["--context", CONTEXT_ID]).success();
    let path = f.record_path(CONTEXT_ID);
    let before = record_message_ids(&path);
    assert_eq!(before.len(), 4);

    let out = stdout_of(conversation(&f.home, &["truncate", CONTEXT_ID, "--keep", "2"]).success());
    assert!(out.contains("dropped 2 messages"), "{out}");

    let lines = record_lines(&path);
    assert_eq!(lines.len(), 3, "a header plus two messages: {lines:?}");
    let header: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(header["type"], "murmur.record");
    assert_eq!(header["truncated"]["dropped"], 2);
    assert_eq!(record_message_ids(&path), before[2..].to_vec());

    let out = stdout_of(conversation(&f.home, &["rm", CONTEXT_ID]).success());
    assert!(out.contains("2 messages"), "{out}");
    assert!(!path.parent().unwrap().exists());
    drop(f.project);
}

/// The refusals: a context id under two stores names both, an absent one names what was looked
/// for and where, and `--keep 0` says that truncating to nothing is `rm`.
#[test]
fn the_commands_refuse_ambiguity_and_absence_by_name() {
    let f = Fixture::new(responses(2), "");
    let a = write_manifest(
        f.project.path(),
        "store-a",
        &f._server.endpoint,
        "context:\n  record_store: store-a\n",
    );
    let b = write_manifest(
        f.project.path(),
        "store-b",
        &f._server.endpoint,
        "context:\n  record_store: store-b\n",
    );
    run_manifest(&f.home, &a, f.workdir.path(), &["--context", CONTEXT_ID]).success();
    run_manifest(&f.home, &b, f.workdir.path(), &["--context", CONTEXT_ID]).success();

    let err = conversation(&f.home, &["rm", CONTEXT_ID]).failure();
    let stderr = String::from_utf8(err.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-CNV-002"), "{stderr}");
    assert!(
        stderr.contains("store-a") && stderr.contains("store-b"),
        "{stderr}"
    );
    assert!(stderr.contains("--record"), "{stderr}");

    // With `--record` it acts on exactly that store.
    conversation(&f.home, &["rm", CONTEXT_ID, "--record", "store-a"]).success();
    assert!(!record_path_in(&f.home, "store-a", CONTEXT_ID).exists());
    assert!(record_path_in(&f.home, "store-b", CONTEXT_ID).exists());

    let err = conversation(&f.home, &["rm", "ctx_nowhere"]).failure();
    let stderr = String::from_utf8(err.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-CNV-001"), "{stderr}");
    assert!(stderr.contains("ctx_nowhere"), "{stderr}");
    assert!(stderr.contains("conversations"), "{stderr}");

    let err = conversation(
        &f.home,
        &["truncate", CONTEXT_ID, "--keep", "0", "--record", "store-b"],
    )
    .failure();
    let stderr = String::from_utf8(err.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-CNV-003"), "{stderr}");
    assert!(stderr.contains("conversation rm"), "{stderr}");
    drop(f.project);
}

// ── A pruned session does not cost the conversation ────────────────────────

/// A session directory is a debugging artefact; the record is the agent's memory. After
/// `max_sessions: 1` has removed the first of two runs, resuming that session is refused with a
/// diagnostic naming it — not a panic, not a silent fresh start — and the record still holds both
/// runs' messages.
#[test]
fn pruning_a_session_does_not_cost_the_conversation_it_ran() {
    let f = Fixture::new(
        responses(2),
        "trace:\n  capture: meta\n  retain:\n    max_sessions: 1\n",
    );

    f.run(&["--context", CONTEXT_ID]).success();
    let first = sessions(&f.sessions_root()).pop().unwrap();
    f.run(&["--context", CONTEXT_ID]).success();

    let survivors = sessions(&f.sessions_root());
    assert_eq!(survivors.len(), 1);
    assert_ne!(survivors[0], first);

    let assert = f.run(&["--resume", &first]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(&first) || stderr.contains("session"),
        "the refusal has to name what is gone: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");

    assert_eq!(
        record_message_ids(&f.record_path(CONTEXT_ID)).len(),
        4,
        "both runs' messages are still in the record"
    );
    drop(f.project);
}

// ── trace.records ──────────────────────────────────────────────────────────

/// The one session a fixture ran, by its records directory.
fn only_session(f: &Fixture) -> PathBuf {
    let names = sessions(&f.sessions_root());
    assert_eq!(names.len(), 1, "{names:?}");
    f.sessions_root().join(&names[0])
}

/// Every event of one session's trace, in file order.
fn trace_events(records: &Path) -> Vec<Value> {
    fs::read_to_string(records.join("trace.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The first event of `event_type` in a session's trace.
fn event_of<'a>(events: &'a [Value], event_type: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["event_type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} event"))
}

/// The keys of one event object, sorted.
fn keys_of(event: &Value) -> Vec<String> {
    let mut keys: Vec<String> = event.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

fn trace_show(f: &Fixture, records: &Path) -> String {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", f.home.path());
    stdout_of(
        cmd.args([
            "trace",
            "show",
            records.join("trace.jsonl").to_str().unwrap(),
        ])
        .assert()
        .success(),
    )
}

/// The record is chained line by line, sealed, and `mur trace verify --json` reports it verified.
fn assert_sealed_and_verified(f: &Fixture, records: &Path) {
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let mut prev = "genesis".to_string();
    for line in trace.lines() {
        assert!(
            line.ends_with(&format!(",\"prev_hash\":\"{prev}\"}}")),
            "the line does not end with prev_hash {prev}: {line}"
        );
        prev = capsule_runtime::record_seal::line_hash(line.as_bytes());
    }
    assert!(records.join("trace.jsonl.seal").is_file());
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", f.home.path());
    let report: Value = serde_json::from_str(&stdout_of(
        cmd.args(["trace", "verify", records.to_str().unwrap(), "--json"])
            .assert()
            .success(),
    ))
    .unwrap();
    assert_eq!(report["verdict"], "verified", "{report}");
}

/// The raw text of `session_start.effective_grants`'s value in a full record: the bytes from the
/// key to the key that follows it, which a full line always writes next.
fn effective_grants_text(records: &Path) -> String {
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let line = trace.lines().next().unwrap();
    let key = "\"effective_grants\":";
    let start = line.find(key).expect("a full record has effective_grants") + key.len();
    let end = start
        + line[start..]
            .find(",\"system_prompt_source\":")
            .expect("system_prompt_source follows effective_grants");
    let text = &line[start..end];
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        serde_json::from_str::<Value>(line).unwrap()["effective_grants"]
    );
    text.to_string()
}

/// An unknown `trace.records` value is refused at parse time naming the field; a typo of the key
/// is an unknown key like any other.
#[test]
fn trace_records_refuses_an_unknown_value_and_warns_on_a_typo() {
    let f = Fixture::new(responses(1), "trace:\n  records: heavy\n");
    let assert = f.run(&[]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-MAN-003"), "{stderr}");
    assert!(stderr.contains("trace.records"), "{stderr}");
    assert!(stderr.contains("heavy"), "{stderr}");
    assert!(sessions(&f.sessions_root()).is_empty());

    let f = Fixture::new(responses(1), "trace: {recrods: light}\n");
    let assert = f.run(&[]).success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("W-SEC-019"), "{stderr}");
}

/// No `trace.records` and `trace.records: full` write the same record: the whole grant set, no
/// light keys, `home/` and `tmp/` kept, and no `records:` line in `mur trace show`.
#[test]
fn trace_records_default_and_full_write_what_main_writes() {
    let mut key_sets = Vec::new();
    for blocks in ["", "trace:\n  records: full\n"] {
        let f = Fixture::new(responses(1), blocks);
        f.run(&[]).success();
        let records = only_session(&f);
        let events = trace_events(&records);
        let start = event_of(&events, "session_start");
        assert!(start["effective_grants"].is_object(), "{blocks:?}: {start}");
        for absent in ["effective_grants_sha256", "trace_records", "unconfined"] {
            assert!(
                start.get(absent).is_none(),
                "{blocks:?}: {absent} in {start}"
            );
        }
        key_sets.push(keys_of(start));
        assert!(records.join("home").is_dir(), "{blocks:?}");
        assert!(records.join("tmp").is_dir(), "{blocks:?}");
        let shown = trace_show(&f, &records);
        assert!(
            !shown.lines().any(|line| line.starts_with("records:")),
            "{blocks:?}: {shown}"
        );
        assert_sealed_and_verified(&f, &records);
        drop(f.project);
    }
    assert_eq!(key_sets[0], key_sets[1]);
}

/// `trace.records: light` keeps the grant set as its digest, says so in the chained and sealed
/// `session_start`, removes the empty `home/` and `tmp/`, and leaves every other event as a full
/// session of the same capsule writes it.
#[test]
fn trace_records_light_keeps_a_digest_and_says_so() {
    let full = Fixture::new(responses(1), "trace:\n  records: full\n");
    full.run(&[]).success();
    let full_records = only_session(&full);
    let full_events = trace_events(&full_records);

    let light = Fixture::new(responses(1), "trace:\n  records: light\n");
    light.run(&[]).success();
    let records = only_session(&light);
    let events = trace_events(&records);
    let start = event_of(&events, "session_start");

    assert_eq!(start["trace_records"], "light", "{start}");
    assert!(start.get("effective_grants").is_none(), "{start}");
    let digest = start["effective_grants_sha256"].as_str().unwrap();
    assert_eq!(digest.len(), 64, "{digest}");
    assert!(
        digest
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "{digest}"
    );
    let full_start = event_of(&full_events, "session_start");
    for mirror in [
        "containment_declared",
        "containment_achieved",
        "workdir_exec",
        "userns_grant",
    ] {
        assert!(start.get(mirror).is_some(), "{mirror}: {start}");
        assert_eq!(start[mirror], full_start[mirror], "{mirror}");
    }
    for event_type in ["task_start", "inference", "task_end", "session_end"] {
        assert_eq!(
            keys_of(event_of(&events, event_type)),
            keys_of(event_of(&full_events, event_type)),
            "{event_type}"
        );
    }
    assert!(records.join("out/result.txt").is_file());
    assert!(!records.join("home").exists());
    assert!(!records.join("tmp").exists());

    let shown = trace_show(&light, &records);
    let line = shown
        .lines()
        .find(|line| line.starts_with("records:"))
        .unwrap_or_else(|| panic!("no records: line: {shown}"));
    assert!(line.contains("light"), "{line}");
    assert!(line.contains(&digest[..12]), "{line}");

    assert_sealed_and_verified(&light, &records);
    assert_sealed_and_verified(&full, &full_records);
    drop(full.project);
    drop(light.project);
}

/// Apparent and allocated bytes and entry counts beneath one records directory.
#[derive(Debug, Default)]
struct RecordsSize {
    apparent: u64,
    allocated: u64,
    files: u64,
    dirs: u64,
    /// Every entry's path relative to the records directory, directories with a trailing `/`.
    entries: Vec<String>,
}

fn records_size(dir: &Path) -> RecordsSize {
    use std::os::unix::fs::MetadataExt;
    let mut size = RecordsSize::default();
    let root = dir;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            size.allocated += meta.blocks() * 512;
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .display()
                .to_string();
            if meta.is_dir() {
                size.entries.push(format!("{rel}/"));
                size.dirs += 1;
                stack.push(entry.path());
            } else {
                size.entries.push(rel);
                size.files += 1;
                if meta.is_file() {
                    size.apparent += meta.len();
                }
            }
        }
    }
    size.entries.sort();
    size
}

/// `du`'s own account of a records directory, for the evidence; `du` absent prints that instead.
fn du_line(dir: &Path) -> String {
    ["-sb", "-sB1"]
        .iter()
        .map(|flag| {
            std::process::Command::new("du")
                .arg(flag)
                .arg(dir)
                .output()
                .map(|out| format!("du {flag}: {}", String::from_utf8_lossy(&out.stdout).trim()))
                .unwrap_or_else(|error| format!("du {flag}: unavailable ({error})"))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// On a one-turn capsule, a light record is smaller than the default one by about the grant
/// set's text, has two fewer directories (`home/`, `tmp/`) and the same files.
///
/// The light line adds `effective_grants_sha256` and `trace_records` (117 bytes with their
/// commas) where the full one has `"effective_grants":` and its value, and the two runs'
/// durations may differ in digit count, so the margin is 200 bytes.
#[test]
fn trace_records_light_is_smaller_on_a_one_turn_capsule() {
    let default = Fixture::new(responses(1), "");
    default.run(&[]).success();
    let default_records = only_session(&default);
    let grants_len = effective_grants_text(&default_records).len() as u64;

    let light = Fixture::new(responses(1), "trace:\n  records: light\n");
    light.run(&[]).success();
    let light_records = only_session(&light);

    let full = records_size(&default_records);
    let small = records_size(&light_records);
    eprintln!(
        "records-size default: apparent={} allocated={} files={} dirs={} ({})",
        full.apparent,
        full.allocated,
        full.files,
        full.dirs,
        du_line(&default_records)
    );
    eprintln!(
        "records-size light:   apparent={} allocated={} files={} dirs={} ({})",
        small.apparent,
        small.allocated,
        small.files,
        small.dirs,
        du_line(&light_records)
    );
    eprintln!("records-size default entries: {}", full.entries.join(" "));
    eprintln!("records-size light entries:   {}", small.entries.join(" "));
    eprintln!("records-size effective_grants value: {grants_len} bytes");
    assert!(
        small.apparent + grants_len.saturating_sub(200) <= full.apparent,
        "light {small:?} vs default {full:?}, grant text {grants_len}"
    );
    assert_eq!(
        small.dirs + 2,
        full.dirs,
        "light {small:?} vs default {full:?}"
    );
    assert_eq!(
        small.files, full.files,
        "light {small:?} vs default {full:?}"
    );
    drop(default.project);
    drop(light.project);
}
