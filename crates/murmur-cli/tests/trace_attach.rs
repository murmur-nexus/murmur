//! An outside party can attach a judgment to a sealed record without editing it: `mur trace
//! attach` verifies the record, then writes one file under `attachments/` naming the sealed head
//! it judges, and `mur trace verify` lists every attachment with whether it points at the head
//! that verified.
//!
//! The model is scripted against the anthropic driver fixture. What runs is real: the `mur`
//! binary, its staging, the kernel tier this host reaches, and the host key it creates under a
//! scratch `HOME`.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::record_seal::{self, line_hash};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "attach-capsule";
const TASK: &str = "List the work folder and report.";
const SHELL_ALLOW: &[&str] = &["bash", "ls"];
const ATTACHMENT_KEYS: [&str; 8] = [
    "format",
    "attachment_id",
    "session_id",
    "evaluator",
    "value",
    "events",
    "attached_at",
    "record",
];

// ── provider replies ─────────────────────────────────────────────────────────

fn tool_call(index: usize, id: &str, command: &str) -> String {
    json!({
        "id": format!("msg_{index}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": id, "name": "bash", "input": {"command": command}}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── project ──────────────────────────────────────────────────────────────────

struct Project {
    home: TempDir,
    _staging: TempDir,
    project: TempDir,
    workdir: TempDir,
    manifest: PathBuf,
    _server: common::ScriptedServer,
}

/// A capsule on the scripted driver that runs one `ls` and answers.
fn project() -> Project {
    let server =
        common::ScriptedServer::start(vec![tool_call(1, "toolu_ls", "ls"), end_turn("done")]);
    let endpoint = server.endpoint.clone();
    let home = TempDir::new().unwrap();
    let staging = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let workdir = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        staging.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let shell = SHELL_ALLOW
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();
    let manifest = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    \
         allow:\n{shell}inference:\n  transport: http\n  model: test-model\n  max_turns: 20\n  \
         driver:\n    artifact: {DRIVER_NAME}\n"
    );
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    Project {
        home,
        _staging: staging,
        project,
        workdir,
        manifest: manifest_path,
        _server: server,
    }
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home);
    command
}

impl Project {
    /// `mur run` to the end, returning the session's records directory.
    fn run(&self, context: &str) -> PathBuf {
        let assert = mur(self.home.path())
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                TASK,
                "--workdir",
                self.workdir.path().to_str().unwrap(),
                "--context",
                context,
                "--containment",
                "advisory",
                "--verbose",
            ])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        common::parse_records_from_stdout(&stdout)
    }

    fn murmur_home(&self) -> PathBuf {
        self.home.path().join(".murmur")
    }

    fn conversation(&self, context: &str) -> PathBuf {
        self.murmur_home()
            .join("conversations")
            .join(CAPSULE_NAME)
            .join(context)
            .join("conversation.jsonl")
    }

    fn attach(&self, records: &Path, args: &[&str]) -> Assert {
        mur(self.home.path())
            .args(["trace", "attach", records.to_str().unwrap()])
            .args(args)
            .assert()
    }

    fn verify(&self, records: &Path, extra: &[&str]) -> Assert {
        mur(self.home.path())
            .args(["trace", "verify", records.to_str().unwrap()])
            .args(extra)
            .assert()
    }

    fn verify_json(&self, records: &Path) -> Value {
        report_of(&self.verify(records, &["--json"]))
    }
}

fn report_of(assert: &Assert) -> Value {
    let stdout = &assert.get_output().stdout;
    serde_json::from_slice(stdout).unwrap_or_else(|error| {
        panic!(
            "--json printed no report ({error}): {}",
            String::from_utf8_lossy(stdout)
        )
    })
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn skip(test: &str) -> bool {
    common::skip_without_host_support(test)
}

/// Line `n` (1-based) of `records`' `trace.jsonl`, parsed.
fn trace_line(records: &Path, n: usize) -> Value {
    let text = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    serde_json::from_str(text.lines().nth(n - 1).unwrap()).unwrap()
}

fn session_id(records: &Path) -> String {
    trace_line(records, 1)["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn trace_seal(records: &Path) -> Value {
    serde_json::from_slice(&fs::read(records.join("trace.jsonl.seal")).unwrap()).unwrap()
}

/// Replaces the first `"event_id":"evt_<hex>` hex digit on `line` (1-based) of `path` with
/// another hex digit: one byte inside a string value.
fn flip_event_id_byte(path: &Path, line: usize) -> Vec<u8> {
    let original = fs::read(path).unwrap();
    let text = String::from_utf8(original.clone()).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let target = &mut lines[line - 1];
    let at = target.find("\"event_id\":\"evt_").expect("an event id") + "\"event_id\":\"evt_".len();
    let byte = target.as_bytes()[at];
    let replacement = if byte == b'a' { "b" } else { "a" };
    target.replace_range(at..=at, replacement);
    fs::write(path, lines.join("\n") + "\n").unwrap();
    original
}

/// Replaces the first occurrence of `from` in `path` with `to`.
fn replace_in(path: &Path, from: &str, to: &str) {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains(from), "{from} is not in {}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
}

/// Every regular file beneath `dir`, recursively.
fn files_under(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            files_under(&path, found);
        } else if kind.is_file() {
            found.push(path);
        }
    }
}

/// The bytes of every file of `records` and of the conversation record beside it.
fn snapshot(records: &Path, conversation: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = Vec::new();
    files_under(records, &mut files);
    files.push(conversation.to_path_buf());
    files.push(conversation.with_file_name("conversation.jsonl.seal"));
    files
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_dir(&entry.path(), &target);
        } else if kind.is_file() {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The attachment files under `records`.
fn attachment_files(records: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    files_under(&records.join("attachments"), &mut files);
    files.sort();
    files
}

/// The single attachment file under `records`, parsed.
fn only_attachment(records: &Path) -> (PathBuf, Value) {
    let files = attachment_files(records);
    assert_eq!(files.len(), 1, "{files:?}");
    let value = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    (files[0].clone(), value)
}

/// The `attachments:` row of `text` that names `id`.
fn attachment_row<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|line| line.trim_start().starts_with(id))
        .unwrap_or_else(|| panic!("no row for {id}: {text}"))
}

fn assert_no_attachments(records: &Path, after: &str) {
    assert!(
        fs::symlink_metadata(records.join("attachments")).is_err(),
        "attachments/ exists after {after}"
    );
}

fn temp_files(records: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    files_under(records, &mut files);
    files.retain(|path| path.to_string_lossy().ends_with(".tmp"));
    files
}

fn attach_id(stdout: &str) -> String {
    stdout
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no attachment id in {stdout}"))
        .to_string()
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// Attaching writes one new file and changes no byte of the record, its seals or its
/// conversation; the attachment names the sealed head and verify lists it as `matches`.
#[test]
#[allow(clippy::print_stdout)]
fn attach_leaves_the_record_unchanged() {
    if skip("attach_leaves_the_record_unchanged") {
        return;
    }
    let p = project();
    let records = p.run("ctx-attach");
    let before_report = p.verify_json(&records);
    assert_eq!(before_report["verdict"], "verified", "{before_report:#}");
    assert_eq!(before_report["schema"], 1);
    assert_eq!(before_report["attachments"], json!([]), "{before_report:#}");
    let before_text = stdout_of(&p.verify(&records, &[]).success());
    assert!(before_text.contains("attachments: none"), "{before_text}");

    let conversation = p.conversation("ctx-attach");
    let before = snapshot(&records, &conversation);
    let session_start = trace_line(&records, 1);
    assert_eq!(session_start["event_type"], "session_start");
    let event = session_start["event_id"].as_str().unwrap().to_string();

    let attached = p
        .attach(
            &records,
            &["--evaluator", "ci", "--value", "0.75", "--event", &event],
        )
        .success();
    let attach_text = stdout_of(&attached);
    println!("── attach\n{attach_text}");

    let after = snapshot(&records, &conversation);
    let added: Vec<&PathBuf> = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .collect();
    assert_eq!(added.len(), 1, "{added:?}");
    let file = added[0];
    let name = file.file_name().unwrap().to_str().unwrap();
    assert_eq!(file.parent().unwrap(), records.join("attachments"));
    let hex = name
        .strip_prefix("att_")
        .and_then(|rest| rest.strip_suffix(".json"))
        .unwrap_or_else(|| panic!("{name}"));
    assert_eq!(hex.len(), 32, "{name}");
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "{} changed", path.display());
    }

    let attachment: Value = serde_json::from_slice(&after[file]).unwrap();
    let keys: Vec<&str> = attachment
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected = ATTACHMENT_KEYS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected, "{attachment:#}");
    let raw = String::from_utf8(after[file].clone()).unwrap();
    let positions: Vec<usize> = ATTACHMENT_KEYS
        .iter()
        .map(|key| raw.find(&format!("\"{key}\"")).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{raw}");
    let seal = trace_seal(&records);
    let id = format!("att_{hex}");
    assert_eq!(attachment["format"], "murmur.attachment/1");
    assert_eq!(attachment["attachment_id"], id.as_str());
    assert_eq!(
        attachment["session_id"].as_str().unwrap(),
        session_id(&records)
    );
    assert_eq!(attachment["evaluator"], "ci");
    assert_eq!(attachment["value"], "0.75");
    assert_eq!(attachment["events"], json!([event]));
    assert!(attachment["attached_at"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(
        attachment["record"],
        json!({
            "stream": "trace.jsonl",
            "head": seal["head"],
            "lines": seal["lines"],
            "key_id": seal["key_id"],
        })
    );
    assert_eq!(
        attach_text.lines().next().unwrap(),
        format!(
            "attached {id} to session {} (trace.jsonl line {}, head {})",
            session_id(&records),
            seal["lines"],
            seal["head"].as_str().unwrap()
        )
    );
    assert_eq!(attach_text.lines().nth(1).unwrap(), file.to_str().unwrap());

    let verified = p.verify(&records, &[]).success();
    let text = stdout_of(&verified);
    println!("── verify\n{text}");
    let record_line = text
        .lines()
        .position(|line| line.starts_with("record: verified, checked with key"))
        .unwrap_or_else(|| panic!("{text}"));
    let header = text
        .lines()
        .position(|line| line == "attachments:")
        .unwrap();
    assert!(record_line < header, "{text}");
    let row = attachment_row(&text, &id);
    assert!(row.contains(" ci "), "{row}");
    assert!(row.contains(" matches "), "{row}");
    assert!(row.contains(" 0.75 "), "{row}");

    let report = p.verify_json(&records);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["verdict"], before_report["verdict"]);
    assert_eq!(report["streams"], before_report["streams"]);
    let listed = &report["attachments"][0];
    assert_eq!(report["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(listed["status"], "matches", "{listed:#}");
    assert_eq!(listed["value"], "0.75");
    assert_eq!(listed["record_head"], seal["head"]);
    assert_eq!(listed["attachment_id"], id.as_str());
    assert_eq!(listed["evaluator"], "ci");
    assert_eq!(listed["events"], json!([event]));
    assert_eq!(listed["attached_at"], attachment["attached_at"]);
    assert_eq!(listed["file"], format!("attachments/{name}"));
    assert!(listed["detail"].is_string());

    // Verifying again changes nothing either.
    assert_eq!(snapshot(&records, &conversation), after);
}

/// A value is stored and listed byte for byte, whatever it holds.
#[test]
#[allow(clippy::print_stdout)]
fn the_value_is_kept_verbatim() {
    if skip("the_value_is_kept_verbatim") {
        return;
    }
    let p = project();
    let records = p.run("ctx-verbatim");
    let scratch = TempDir::new().unwrap();
    let multi_line = "{\n  \"score\": 1,   \n  \"notes\": \"two\\nlines\"  \n}  ";
    let value_file = scratch.path().join("value.json");
    fs::write(&value_file, multi_line).unwrap();
    let unterminated = "{\"score\": 1, \"notes\": \"unterminated";

    let mut ids = Vec::new();
    for args in [
        vec!["--value", "pass"],
        vec!["--value", unterminated],
        vec!["--value-file", value_file.to_str().unwrap()],
    ] {
        let mut full = vec!["--evaluator", "reviewer"];
        full.extend(args);
        ids.push(attach_id(&stdout_of(&p.attach(&records, &full).success())));
    }
    let piped = mur(p.home.path())
        .args([
            "trace",
            "attach",
            records.to_str().unwrap(),
            "--evaluator",
            "model",
            "--value-file",
            "-",
        ])
        .write_stdin("  0.5\n")
        .assert()
        .success();
    ids.push(attach_id(&stdout_of(&piped)));
    let values = ["pass", unterminated, multi_line, "  0.5\n"];

    for (id, value) in ids.iter().zip(values) {
        let stored: Value = serde_json::from_slice(
            &fs::read(records.join(format!("attachments/{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            stored["value"].as_str().unwrap().as_bytes(),
            value.as_bytes()
        );
    }

    let report = p.verify_json(&records);
    let listed = report["attachments"].as_array().unwrap();
    assert_eq!(listed.len(), 4, "{report:#}");
    for ((entry, id), value) in listed.iter().zip(&ids).zip(values) {
        assert_eq!(entry["attachment_id"], id.as_str(), "{report:#}");
        assert_eq!(entry["status"], "matches", "{entry:#}");
        assert_eq!(
            entry["value"].as_str().unwrap().as_bytes(),
            value.as_bytes()
        );
    }
    let times: Vec<u64> = listed
        .iter()
        .map(|entry| entry["attached_at"].as_u64().unwrap())
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");

    let text = stdout_of(&p.verify(&records, &[]).success());
    println!("── verify\n{text}");
    assert!(attachment_row(&text, &ids[0]).contains(" pass "), "{text}");
    assert!(
        attachment_row(&text, &ids[1]).contains(unterminated),
        "{text}"
    );
    assert!(
        attachment_row(&text, &ids[2])
            .contains(&format!("<{} bytes, see --json>", multi_line.len())),
        "{text}"
    );
    assert!(
        attachment_row(&text, &ids[3]).contains("<6 bytes, see --json>"),
        "{text}"
    );
}

/// A record changed after the attachment fails verify exactly as before and lists the attachment
/// as `unverified`; restored, it verifies and the attachment matches again.
#[test]
#[allow(clippy::print_stdout)]
fn a_changed_record_fails_and_lists_its_attachments() {
    if skip("a_changed_record_fails_and_lists_its_attachments") {
        return;
    }
    let p = project();
    let records = p.run("ctx-changed");
    let id = attach_id(&stdout_of(
        &p.attach(&records, &["--evaluator", "ci", "--value", "0.75"])
            .success(),
    ));
    let trace = records.join("trace.jsonl");
    let original = flip_event_id_byte(&trace, 2);

    let failed = p.verify(&records, &[]).failure().code(1);
    let (text, error) = (stdout_of(&failed), stderr_of(&failed));
    println!("── changed\n{text}{error}");
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(error.contains("trace.jsonl line 2"), "{error}");
    assert!(text.contains("record: changed"), "{text}");
    assert!(
        attachment_row(&text, &id).contains(" unverified "),
        "{text}"
    );
    let report = report_of(&p.verify(&records, &["--json"]).failure().code(1));
    assert_eq!(report["verdict"], "changed");
    assert_eq!(report["attachments"][0]["status"], "unverified");
    assert_eq!(
        report["attachments"][0]["detail"],
        "the record did not verify: changed"
    );

    fs::write(&trace, original).unwrap();
    let text = stdout_of(&p.verify(&records, &[]).success());
    assert!(attachment_row(&text, &id).contains(" matches "), "{text}");
}

/// An attachment that names another head reads `mismatch` (naming the earlier line when it is
/// one), and one that is not JSON reads `unreadable`; the record's verdict and exit status stay.
#[test]
#[allow(clippy::print_stdout)]
fn a_changed_head_reads_mismatch() {
    if skip("a_changed_head_reads_mismatch") {
        return;
    }
    let p = project();
    let records = p.run("ctx-mismatch");
    p.attach(&records, &["--evaluator", "ci", "--value", "0.75"])
        .success();
    let baseline = p.verify_json(&records);
    let (file, attachment) = only_attachment(&records);
    let seal = trace_seal(&records);
    let verified_head = seal["head"].as_str().unwrap().to_string();
    let lines = seal["lines"].as_u64().unwrap();
    assert!(lines > 2, "{seal:#}");

    // (a) a head of no line.
    let foreign = format!("sha256:{}", "ab".repeat(32));
    replace_in(&file, &verified_head, &foreign);
    let report = report_of(&p.verify(&records, &["--json"]).success());
    assert_eq!(report["streams"], baseline["streams"]);
    assert_eq!(report["verdict"], "verified");
    let entry = &report["attachments"][0];
    assert_eq!(entry["status"], "mismatch", "{entry:#}");
    assert_eq!(entry["record_head"], foreign.as_str());
    let detail = entry["detail"].as_str().unwrap();
    assert!(detail.contains(&foreign), "{detail}");
    assert!(detail.contains(&verified_head), "{detail}");
    let text = stdout_of(&p.verify(&records, &[]).success());
    println!("── foreign head\n{text}");
    assert!(
        attachment_row(&text, attachment["attachment_id"].as_str().unwrap()).contains(" mismatch "),
        "{text}"
    );

    // (b) the head of line 2.
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let line_two = line_hash(trace.lines().nth(1).unwrap().as_bytes());
    replace_in(&file, &foreign, &line_two);
    let report = report_of(&p.verify(&records, &["--json"]).success());
    let entry = &report["attachments"][0];
    assert_eq!(entry["status"], "mismatch", "{entry:#}");
    let detail = entry["detail"].as_str().unwrap();
    println!("── line 2 head: {detail}");
    assert!(
        detail.contains(&format!(
            "names line 2 of a trace that now holds {lines} lines"
        )),
        "{detail}"
    );
    assert!(detail.contains(&verified_head), "{detail}");

    // (c) not JSON.
    fs::write(&file, "not json").unwrap();
    let report = report_of(&p.verify(&records, &["--json"]).success());
    assert_eq!(report["streams"], baseline["streams"]);
    let entry = &report["attachments"][0];
    assert_eq!(entry["status"], "unreadable", "{entry:#}");
    let name = format!(
        "attachments/{}",
        file.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(entry["file"], name.as_str());
    let detail = entry["detail"].as_str().unwrap();
    assert!(
        detail.starts_with(&format!("{name} is not JSON: ")),
        "{detail}"
    );
    for key in [
        "attachment_id",
        "evaluator",
        "value",
        "events",
        "attached_at",
        "record_head",
    ] {
        assert!(entry[key].is_null(), "{key} in {entry:#}");
    }
    let text = stdout_of(&p.verify(&records, &[]).success());
    println!("── not json\n{text}");
    assert!(
        attachment_row(&text, &name).contains(" unreadable "),
        "{text}"
    );
}

/// `mur trace attach` refuses, writing nothing, unless the record verifies, with the code
/// `mur trace verify` exits with; another host's record attaches with that host's public key.
#[test]
#[allow(clippy::print_stdout)]
fn attach_refuses_an_unverified_record() {
    if skip("attach_refuses_an_unverified_record") {
        return;
    }
    let p = project();
    let records = p.run("ctx-refuse");
    let prefix = format!("cannot attach to session {}:", session_id(&records));
    let args = ["--evaluator", "ci", "--value", "0.75"];
    let refused = |code: &str, case: &str| {
        let assert = p.attach(&records, &args).failure().code(1);
        let error = stderr_of(&assert);
        println!("── {case}\n{error}");
        assert!(
            error.contains(&format!("error[{code}]: {prefix} ")),
            "{case}: {error}"
        );
        assert_no_attachments(&records, case);
        error
    };

    // (a) a changed trace line.
    let trace = records.join("trace.jsonl");
    let original = flip_event_id_byte(&trace, 2);
    let error = refused("E-TRC-003", "changed");
    assert!(error.contains("trace.jsonl line 2"), "{error}");
    fs::write(&trace, original).unwrap();

    // (b) no seal.
    let seal = records.join("trace.jsonl.seal");
    let seal_bytes = fs::read(&seal).unwrap();
    fs::remove_file(&seal).unwrap();
    refused("E-TRC-004", "unsealed");
    fs::write(&seal, seal_bytes).unwrap();

    // (c) no public key.
    let public = p.murmur_home().join("keys/record-seal.pub");
    let public_bytes = fs::read(&public).unwrap();
    fs::remove_file(&public).unwrap();
    refused("E-TRC-005", "no public key");
    fs::write(&public, public_bytes).unwrap();

    // (d) host A's record, copied to host B.
    let b_home = TempDir::new().unwrap();
    let b_key = record_seal::HostKey::load_or_create(&b_home.path().join(".murmur")).unwrap();
    let elsewhere = TempDir::new().unwrap();
    let copy = elsewhere.path().join("copied-record");
    copy_dir(&records, &copy);
    assert_no_attachments(&copy, "copying");
    let a_pub = elsewhere.path().join("a.pub");
    fs::copy(&public, &a_pub).unwrap();
    let a_id = record_seal::PublicKey::load(&a_pub)
        .unwrap()
        .key_id()
        .to_string();
    assert_ne!(a_id, b_key.key_id());

    let foreign = mur(b_home.path())
        .args(["trace", "attach", copy.to_str().unwrap()])
        .args(args)
        .assert()
        .failure()
        .code(1);
    let error = stderr_of(&foreign);
    println!("── B without A's key\n{error}");
    assert!(
        error.contains(&format!("error[E-TRC-005]: {prefix} ")),
        "{error}"
    );
    assert_no_attachments(&copy, "B without A's key");

    mur(b_home.path())
        .args(["trace", "attach", copy.to_str().unwrap()])
        .args(args)
        .args(["--public-key", a_pub.to_str().unwrap()])
        .assert()
        .success();
    let (_, attachment) = only_attachment(&copy);
    assert_eq!(attachment["record"]["key_id"], a_id.as_str());
    let verified = mur(b_home.path())
        .args([
            "trace",
            "verify",
            copy.to_str().unwrap(),
            "--public-key",
            a_pub.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();
    let report = report_of(&verified);
    assert_eq!(report["attachments"][0]["status"], "matches", "{report:#}");
    assert_no_attachments(&records, "attaching to the copy");
    p.verify(&records, &[]).success();
}

/// Bad input is refused before anything is written, naming the flag.
#[test]
#[allow(clippy::print_stdout)]
fn attach_refuses_bad_input() {
    if skip("attach_refuses_bad_input") {
        return;
    }
    let p = project();
    let records = p.run("ctx-input");
    let scratch = TempDir::new().unwrap();
    let not_utf8 = scratch.path().join("latin1.txt");
    fs::write(&not_utf8, b"caf\xe9").unwrap();
    let missing = scratch.path().join("missing.txt");
    let unknown_event = "evt_00000000000000000000000000000000";

    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                "--evaluator",
                "ci",
                "--value",
                "1",
                "--event",
                unknown_event,
            ],
            unknown_event,
        ),
        (vec!["--evaluator", "ci", "--value", ""], "--value is empty"),
        (
            vec!["--evaluator", "", "--value", "1"],
            "--evaluator is empty",
        ),
        (
            vec!["--evaluator", "c\ni", "--value", "1"],
            "holds a control character",
        ),
        (
            vec![
                "--evaluator",
                "ci",
                "--value-file",
                missing.to_str().unwrap(),
            ],
            "cannot be read",
        ),
        (
            vec![
                "--evaluator",
                "ci",
                "--value-file",
                not_utf8.to_str().unwrap(),
            ],
            "is not UTF-8",
        ),
    ];
    for (args, reason) in cases {
        let assert = p.attach(&records, &args).failure().code(1);
        let error = stderr_of(&assert);
        println!("── {args:?}\n{error}");
        assert!(error.contains("error[E-TRC-006]: "), "{args:?}: {error}");
        assert!(error.contains(reason), "{args:?}: {error}");
        let flag = if reason == unknown_event {
            "--event"
        } else if args.contains(&"--value-file") {
            "--value-file"
        } else if args[1].is_empty() || args[1].contains('\n') {
            "--evaluator"
        } else {
            "--value"
        };
        assert!(error.contains(flag), "{args:?}: {error}");
        assert_no_attachments(&records, &format!("{args:?}"));
    }

    // clap refuses these before `mur trace attach` runs.
    for args in [
        vec!["--evaluator", "ci"],
        vec![
            "--evaluator",
            "ci",
            "--value",
            "1",
            "--value-file",
            not_utf8.to_str().unwrap(),
        ],
        vec!["--value", "1"],
    ] {
        let assert = p.attach(&records, &args).failure().code(2);
        println!("── {args:?}\n{}", stderr_of(&assert));
        assert_no_attachments(&records, &format!("{args:?}"));
    }
    p.verify(&records, &[]).success();
}

/// A records directory `mur` cannot write beneath is `E-TRC-007`, leaving no temporary file.
#[test]
#[allow(clippy::print_stdout)]
fn attach_reports_a_write_failure() {
    if skip("attach_reports_a_write_failure") {
        return;
    }
    let p = project();
    if fs::metadata(p.home.path()).unwrap().uid() == 0 {
        eprintln!("[SKIP-HOST] attach_reports_a_write_failure: root writes a read-only directory");
        return;
    }
    let records = p.run("ctx-write");
    let args = ["--evaluator", "ci", "--value", "0.75"];

    // (a) attachments/ is a link.
    let target = TempDir::new().unwrap();
    std::os::unix::fs::symlink(target.path(), records.join("attachments")).unwrap();
    let error = stderr_of(&p.attach(&records, &args).failure().code(1));
    println!("── link\n{error}");
    assert!(error.contains("error[E-TRC-007]: "), "{error}");
    assert!(error.contains(records.to_str().unwrap()), "{error}");
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    fs::remove_file(records.join("attachments")).unwrap();

    // (b) a read-only records directory.
    let mode = fs::metadata(&records).unwrap().permissions().mode();
    fs::set_permissions(&records, fs::Permissions::from_mode(0o555)).unwrap();
    let failed = p.attach(&records, &args).failure().code(1);
    fs::set_permissions(&records, fs::Permissions::from_mode(mode)).unwrap();
    let error = stderr_of(&failed);
    println!("── read-only\n{error}");
    assert!(error.contains("error[E-TRC-007]: "), "{error}");
    assert!(error.contains("attachments"), "{error}");

    assert_no_attachments(&records, "the write failures");
    assert_eq!(temp_files(&records), Vec::<PathBuf>::new());
    p.verify(&records, &[]).success();
}

/// `mur trace attach --json` prints one object: the attachment's keys, in order, then `path`.
#[test]
fn attach_json_is_one_object() {
    if skip("attach_json_is_one_object") {
        return;
    }
    let p = project();
    let records = p.run("ctx-json");
    let assert = p
        .attach(
            &records,
            &[
                "--evaluator",
                "ci",
                "--value",
                "{\"score\": 0.75}",
                "--json",
            ],
        )
        .success();
    let stdout = stdout_of(&assert);
    let object: Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"));
    let mut keys: Vec<&str> = object
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected: Vec<&str> = ATTACHMENT_KEYS.to_vec();
    expected.push("path");
    let order: Vec<usize> = expected
        .iter()
        .map(|key| stdout.find(&format!("\n  \"{key}\"")).unwrap())
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{stdout}");
    expected.sort_unstable();
    assert_eq!(keys, expected, "{stdout}");
    assert_eq!(object["value"], "{\"score\": 0.75}");

    let (file, attachment) = only_attachment(&records);
    assert_eq!(object["path"], file.to_str().unwrap());
    let mut without_path = object.clone();
    without_path.as_object_mut().unwrap().remove("path");
    assert_eq!(without_path, attachment);
}
