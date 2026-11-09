//! A session's records can be checked by anyone holding the host's public key and nothing else:
//! every `trace.jsonl` line chains to the one before it, `mur` seals every stream with the host
//! key when the session ends, and `mur trace verify` names the first line that no longer matches.
//!
//! The model is scripted against the anthropic driver fixture; the `records-hook` WASM fixture
//! writes `hooks/eval.jsonl`. What runs is real: the `mur` binary, its staging, the kernel tier
//! this host reaches, and the host key it creates under a scratch `HOME`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::record_seal::{self, line_hash};
use murmur_artifact::ContainmentClass;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const HOOK_ARTIFACT: &str = "local/records-hook";
const HOOK_VERSION: &str = "0.1.0";
const CAPSULE_NAME: &str = "seal-capsule";
const TASK: &str = "Probe the records directory and report.";
const RECORDS_GRANT: &str = "      filesystem:\n        records: true\n";
const SHELL_ALLOW: &[&str] = &["bash", "cat", "stat", "head", "ls"];

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

/// One `ls` call, then the answer: a session with a handful of lines and a resource of work.
fn ordinary_script() -> Vec<String> {
    vec![tool_call(1, "toolu_ls", "ls"), end_turn("done")]
}

// ── project ──────────────────────────────────────────────────────────────────

struct Project {
    home: TempDir,
    _staging: TempDir,
    project: TempDir,
    workdir: TempDir,
    manifest: PathBuf,
}

fn records_hook_zip(dir: &Path) -> PathBuf {
    let path = dir.join(format!("records-hook-{HOOK_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("records-hook/murmur.yaml")).unwrap())
        .unwrap();
    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("records-hook/hook/records-hook.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    path
}

/// The scripted driver and the `records-hook` fixture granted the records directory, with
/// `extra` appended to the manifest.
fn project(endpoint: &str, extra: &str) -> Project {
    project_in(endpoint, extra, &std::env::temp_dir())
}

/// [`project`] with its home, project and work folder beneath `base`.
fn project_in(endpoint: &str, extra: &str, base: &Path) -> Project {
    let home = TempDir::new_in(base).unwrap();
    let staging = TempDir::new().unwrap();
    let project = TempDir::new_in(base).unwrap();
    let workdir = TempDir::new_in(base).unwrap();
    let driver = common::create_driver_artifact(
        staging.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    common::publish_local(&home, &records_hook_zip(staging.path())).success();
    let shell = SHELL_ALLOW
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();
    let manifest = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\n  - name: {HOOK_ARTIFACT}\n    version: {HOOK_VERSION}\n    runtime: \
         hook\n    capabilities:\n{RECORDS_GRANT}capabilities:\n  network:\n    allow:\n      - \
         {endpoint}\n  shell:\n    allow:\n{shell}inference:\n  transport: http\n  model: \
         test-model\n  max_turns: 20\n  driver:\n    artifact: {DRIVER_NAME}\n{extra}"
    );
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    Project {
        home,
        _staging: staging,
        project,
        workdir,
        manifest: manifest_path,
    }
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home);
    command
}

impl Project {
    fn run_args(&self, context: &str) -> Vec<String> {
        self.run_args_at(context, "advisory")
    }

    fn run_args_at(&self, context: &str, containment: &str) -> Vec<String> {
        vec![
            "run".into(),
            "--manifest".into(),
            self.manifest.display().to_string(),
            "--task".into(),
            TASK.into(),
            "--workdir".into(),
            self.workdir.path().display().to_string(),
            "--context".into(),
            context.into(),
            "--containment".into(),
            containment.into(),
            "--verbose".into(),
        ]
    }

    /// `mur run` to the end, returning the session's records directory and its stderr.
    fn run(&self, context: &str) -> (PathBuf, String) {
        let assert = mur(self.home.path())
            .current_dir(self.project.path())
            .args(self.run_args(context))
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        (common::parse_records_from_stdout(&stdout), stderr)
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

    fn verify(&self, records: &Path, extra: &[&str]) -> Assert {
        mur(self.home.path())
            .args(["trace", "verify", records.to_str().unwrap()])
            .args(extra)
            .assert()
    }

    fn verify_json(&self, records: &Path) -> Value {
        let output = self.verify(records, &["--json"]).get_output().clone();
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "--json printed no report ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn show(&self, records: &Path) -> String {
        let output = mur(self.home.path())
            .args([
                "trace",
                "show",
                records.join("trace.jsonl").to_str().unwrap(),
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(output).unwrap()
    }
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn stream<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["stream"] == name)
        .unwrap_or_else(|| panic!("no {name} stream in {report:#}"))
}

fn skip(test: &str) -> bool {
    common::skip_without_host_support(test)
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

/// Replaces the first occurrence of `from` in `path` with `to`, returning the original bytes.
fn replace_in(path: &Path, from: &str, to: &str) -> Vec<u8> {
    let original = fs::read(path).unwrap();
    let text = String::from_utf8(original.clone()).unwrap();
    assert!(text.contains(from), "{from} is not in {}", path.display());
    fs::write(path, text.replacen(from, to, 1)).unwrap();
    original
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every file beneath `dir`, recursively, skipping `skip`.
fn files_under(dir: &Path, skip: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            files_under(&path, skip, found);
        } else if kind.is_file() {
            found.push(path);
        }
    }
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

// ── scenarios ────────────────────────────────────────────────────────────────

/// A finished session's every stream verifies, every trace line chains, the key
/// files carry their modes, and the key's bytes appear nowhere but in its own file.
#[test]
#[allow(clippy::print_stdout)]
fn finished_session_verifies() {
    if skip("finished_session_verifies") {
        return;
    }
    let server = common::ScriptedServer::start(ordinary_script());
    let p = project(&server.endpoint, "");
    let (records, run_stderr) = p.run("ctx-seal");

    // The key files.
    let keys = p.murmur_home().join("keys");
    assert_eq!(mode(&keys), 0o700);
    assert_eq!(mode(&keys.join("record-seal.key")), 0o600);
    assert_eq!(mode(&keys.join("record-seal.pub")), 0o644);
    let public = fs::read_to_string(keys.join("record-seal.pub")).unwrap();
    assert_eq!(public.len(), 65, "{public:?}");
    let seed = fs::read_to_string(keys.join("record-seal.key")).unwrap();
    let seed = seed.trim();
    assert_eq!(seed.len(), 64);
    let key_id = record_seal::PublicKey::from_hex(&public)
        .unwrap()
        .key_id()
        .to_string();

    // Every trace line ends with prev_hash naming the line before; session_end is last.
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let mut prev = "genesis".to_string();
    let mut last = Value::Null;
    for line in trace.lines() {
        assert!(
            line.ends_with(&format!(",\"prev_hash\":\"{prev}\"}}")),
            "the line does not end with prev_hash {prev}: {line}"
        );
        last = serde_json::from_str(line).unwrap();
        prev = line_hash(line.as_bytes());
    }
    assert_eq!(last["event_type"], "session_end", "{trace}");

    // The three seals, with the documented keys.
    for (seal_path, chain) in [
        (records.join("trace.jsonl.seal"), "inline"),
        (records.join("hooks/eval.jsonl.seal"), "listed"),
        (
            p.conversation("ctx-seal").with_extension("jsonl.seal"),
            "listed",
        ),
    ] {
        let seal: Value = serde_json::from_str(
            &fs::read_to_string(&seal_path)
                .unwrap_or_else(|error| panic!("{}: {error}", seal_path.display())),
        )
        .unwrap();
        println!("── {}\n{seal:#}", seal_path.display());
        assert_eq!(seal["format"], "murmur.seal/1");
        assert_eq!(seal["chain"], chain);
        assert_eq!(seal["key_id"], key_id.as_str());
        assert_eq!(seal["algorithm"], "ed25519");
        assert_eq!(seal["signature"].as_str().unwrap().len(), 128);
        for key in ["stream", "session_id", "lines", "head", "sealed_at"] {
            assert!(!seal[key].is_null(), "{key} missing: {seal:#}");
        }
        assert_eq!(
            seal["line_hashes"].is_array(),
            chain == "listed",
            "{seal:#}"
        );
    }
    let trace_seal: Value =
        serde_json::from_str(&fs::read_to_string(records.join("trace.jsonl.seal")).unwrap())
            .unwrap();
    assert_eq!(trace_seal["head"], prev.as_str());
    assert_eq!(
        trace_seal["conversation"],
        json!({"record": CAPSULE_NAME, "context_id": "ctx-seal"})
    );

    // mur trace verify: every stream verified, exit 0.
    let verified = p.verify(&records, &[]).success();
    let text = stdout_of(&verified);
    println!("── mur trace verify\n{text}");
    for name in [
        "trace.jsonl",
        "hooks/eval.jsonl",
        "conversation:seal-capsule/ctx-seal",
    ] {
        let row = text
            .lines()
            .find(|line| line.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row: {text}"));
        assert!(row.contains("verified"), "{row}");
    }
    assert!(
        text.contains(&format!("record: verified, checked with key {key_id}")),
        "{text}"
    );
    let report = p.verify_json(&records);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["verdict"], "verified");
    assert_eq!(report["public_key"]["key_id"], key_id.as_str());

    let shown = p.show(&records);
    assert!(!shown.contains("record:"), "{shown}");

    // The seed appears in no file outside the key folder, and in no output.
    let mut files = Vec::new();
    files_under(p.home.path(), &keys, &mut files);
    files_under(p.workdir.path(), &keys, &mut files);
    files_under(p.project.path(), &keys, &mut files);
    for file in files {
        let bytes = fs::read(&file).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(seed),
            "the record-seal key leaked into {}",
            file.display()
        );
    }
    for output in [&run_stderr, &text, &shown] {
        assert!(
            !output.contains(seed),
            "the record-seal key leaked into output"
        );
    }
}

/// One changed byte is named by its stream and line; restoring it verifies again.
#[test]
#[allow(clippy::print_stdout)]
fn one_changed_byte_names_the_line() {
    if skip("one_changed_byte_names_the_line") {
        return;
    }
    let server = common::ScriptedServer::start(ordinary_script());
    let p = project(&server.endpoint, "");
    let (records, _) = p.run("ctx-byte");
    p.verify(&records, &[]).success();

    // trace.jsonl line 3.
    let trace = records.join("trace.jsonl");
    let original = flip_event_id_byte(&trace, 3);
    let changed = p.verify(&records, &[]).failure();
    let error = stderr_of(&changed);
    println!("── changed trace\n{}{error}", stdout_of(&changed));
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(error.contains("trace.jsonl line 3"), "{error}");
    let report = p.verify_json(&records);
    assert_eq!(report["verdict"], "changed");
    assert_eq!(stream(&report, "trace.jsonl")["first_bad_line"], 3);
    let shown = p.show(&records);
    let mark = shown
        .lines()
        .find(|line| line.starts_with("record:"))
        .unwrap_or_else(|| panic!("no record: line in {shown}"));
    assert!(mark.contains("changed"), "{mark}");
    assert!(mark.contains("line 3"), "{mark}");
    fs::write(&trace, original).unwrap();
    p.verify(&records, &[]).success();

    // hooks/eval.jsonl line 1.
    let eval = records.join("hooks/eval.jsonl");
    let original = replace_in(&eval, "dataset_run", "dataset_ruN");
    let error = stderr_of(&p.verify(&records, &[]).failure());
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(error.contains("hooks/eval.jsonl line 1"), "{error}");
    let report = p.verify_json(&records);
    assert_eq!(stream(&report, "hooks/eval.jsonl")["first_bad_line"], 1);
    fs::write(&eval, original).unwrap();
    p.verify(&records, &[]).success();

    // One byte of the conversation record.
    let conversation = p.conversation("ctx-byte");
    let original = replace_in(&conversation, "Probe the records", "Prabe the records");
    let error = stderr_of(&p.verify(&records, &[]).failure());
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(
        error.contains("conversation:seal-capsule/ctx-byte line"),
        "{error}"
    );
    fs::write(&conversation, original).unwrap();
    p.verify(&records, &[]).success();
}

/// Cut lines read as unsealed, with where the chain stops and what the seal covers;
/// deleting the seal too reads as no seal. Never verified, never changed.
#[test]
#[allow(clippy::print_stdout)]
fn cut_lines_report_no_seal() {
    if skip("cut_lines_report_no_seal") {
        return;
    }
    let server = common::ScriptedServer::start(ordinary_script());
    let p = project(&server.endpoint, "");
    let (records, _) = p.run("ctx-cut");
    let trace = records.join("trace.jsonl");
    let text = fs::read_to_string(&trace).unwrap();
    let total = text.lines().count();
    let kept = total - 2;
    let cut: Vec<&str> = text.lines().take(kept).collect();
    fs::write(&trace, cut.join("\n") + "\n").unwrap();

    let unsealed = p.verify(&records, &[]).failure();
    let error = stderr_of(&unsealed);
    println!("── cut\n{}{error}", stdout_of(&unsealed));
    assert!(error.contains("E-TRC-004"), "{error}");
    assert!(
        error.contains(&format!("intact through line {kept}")),
        "{error}"
    );
    assert!(error.contains(&format!("covers {total} lines")), "{error}");
    let report = p.verify_json(&records);
    assert_eq!(report["verdict"], "unsealed");
    assert_eq!(stream(&report, "trace.jsonl")["chain_intact_through"], kept);
    assert_eq!(stream(&report, "trace.jsonl")["sealed_lines"], total);

    fs::remove_file(records.join("trace.jsonl.seal")).unwrap();
    let error = stderr_of(&p.verify(&records, &[]).failure());
    assert!(error.contains("E-TRC-004"), "{error}");
    assert!(error.contains("no seal"), "{error}");
    let report = p.verify_json(&records);
    assert_eq!(report["verdict"], "unsealed");
    assert_eq!(stream(&report, "trace.jsonl")["chain_intact_through"], kept);
    assert!(stream(&report, "trace.jsonl")["sealed_lines"].is_null());
}

/// A runtime killed mid-session leaves a record that says it ended without a seal,
/// with the chain intact through its last complete line.
#[test]
#[allow(clippy::print_stdout)]
fn killed_runtime_is_unsealed() {
    if skip("killed_runtime_is_unsealed") {
        return;
    }
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let mut calls = 0;
    let server = common::ScriptedServer::start_answering(8, move |_| {
        calls += 1;
        if calls == 1 {
            return tool_call(1, "toolu_ls", "ls");
        }
        let _ = arrived_tx.send(());
        let _ = release_rx
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(120));
        end_turn("done")
    });
    let p = project(&server.endpoint, "");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", p.home.path())
        .current_dir(p.project.path())
        .args(p.run_args("ctx-kill"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    arrived_rx
        .recv_timeout(std::time::Duration::from_secs(240))
        .expect("the second request arrives");
    common::signal(child.id(), libc::SIGKILL);
    child.wait().unwrap();
    let _ = release_tx.send(());

    let records = common::newest_session_records(p.home.path(), p.workdir.path())
        .expect("the killed session left its records");
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let complete = trace.lines().count();
    assert!(
        trace.contains("\"event_type\":\"session_start\""),
        "{trace}"
    );
    assert!(!trace.contains("\"event_type\":\"session_end\""), "{trace}");
    assert!(!records.join("trace.jsonl.seal").exists());
    let unsealed = p.verify(&records, &[]).failure();
    let error = stderr_of(&unsealed);
    println!("── killed\n{}{error}", stdout_of(&unsealed));
    assert!(error.contains("E-TRC-004"), "{error}");
    assert!(error.contains("ended without a seal"), "{error}");
    assert!(
        error.contains(&format!("intact through line {complete}")),
        "{error}"
    );
    let shown = p.show(&records);
    let mark = shown
        .lines()
        .find(|line| line.starts_with("record:"))
        .unwrap_or_else(|| panic!("no record: line in {shown}"));
    assert!(mark.contains("unsealed"), "{mark}");
}

/// A capsule command at `scoped` or `sealed` cannot read the host's record-seal key, and at
/// `sealed` cannot stat it either. The home is outside `/tmp`, which both tiers replace with a
/// private one, so the refusal is the tier's own: Landlock's `EACCES` at `scoped`, absence from
/// the composed root at `sealed`.
fn commands_cannot_read_the_seal_key(test: &str, floor: ContainmentClass) {
    if skip(test) {
        return;
    }
    let achieved = capsule_runtime::detect_containment_decision(floor)
        .0
        .achieved();
    if achieved < floor {
        eprintln!("[SKIP-HOST] {test}: this host reaches {achieved}, the test needs {floor}");
        return;
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let (arrived_tx, _arrived_rx) = std::sync::mpsc::channel::<()>();
    let probe_for = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let probe = std::sync::Arc::clone(&probe_for);
    let mut calls = 0;
    let server = common::ScriptedServer::start_answering(8, move |_| {
        calls += 1;
        let _ = arrived_tx.send(());
        if calls == 1 {
            tool_call(1, "toolu_key", &probe.lock().unwrap())
        } else {
            end_turn("done")
        }
    });
    let p = project_in(&server.endpoint, "", &base);
    let murmur_home = p.murmur_home();
    fs::create_dir_all(&murmur_home).unwrap();
    record_seal::HostKey::load_or_create(&murmur_home).unwrap();
    let key_path = record_seal::private_key_path(&murmur_home);
    let seed = fs::read_to_string(&key_path).unwrap();
    *probe_for.lock().unwrap() = format!(
        "cat '{0}'; echo \"KEY-CAT|$?\"; stat '{0}'; echo \"KEY-STAT|$?\"",
        key_path.display()
    );
    let assert = mur(p.home.path())
        .current_dir(p.project.path())
        .args(p.run_args_at("ctx-key", &floor.to_string()))
        .assert()
        .success();
    let records = common::parse_records_from_stdout(&stdout_of(&assert));
    let text = common::tool_result_text(&server.requests(), "toolu_key")
        .expect("the probe's result reached the model");
    println!("── {test}\n{text}");
    let status = |label: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(label))
            .unwrap_or_else(|| panic!("no {label} in {text}"))
            .to_string()
    };
    assert_ne!(status("KEY-CAT|"), "0", "reading the key must fail: {text}");
    // Landlock mediates opening a file, not its metadata: at `scoped` a command can stat the key
    // (its existence, size and mode) and never read a byte of it. At `sealed` the path does not
    // exist for the command at all.
    if floor == ContainmentClass::Sealed {
        assert_ne!(status("KEY-STAT|"), "0", "stat must fail at sealed: {text}");
    }
    let refusal = if floor == ContainmentClass::Sealed {
        "No such file or directory"
    } else {
        "Permission denied"
    };
    assert!(text.contains(refusal), "{refusal}: {text}");
    assert!(!text.contains(seed.trim()), "the key reached the capsule");
    // The record of the session that tried still verifies.
    p.verify(&records, &[]).success();
}

#[test]
#[allow(clippy::print_stdout)]
fn commands_cannot_read_the_seal_key_at_scoped() {
    commands_cannot_read_the_seal_key(
        "commands_cannot_read_the_seal_key_at_scoped",
        ContainmentClass::Scoped,
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn commands_cannot_read_the_seal_key_at_sealed() {
    commands_cannot_read_the_seal_key(
        "commands_cannot_read_the_seal_key_at_sealed",
        ContainmentClass::Sealed,
    );
}

/// A record copied off its host verifies anywhere, given only that host's public
/// key; without it, the record names the key that sealed it and the key it was checked with.
#[test]
#[allow(clippy::print_stdout)]
fn another_hosts_record_needs_only_its_public_key() {
    if skip("another_hosts_record_needs_only_its_public_key") {
        return;
    }
    let server_a = common::ScriptedServer::start(ordinary_script());
    let a = project(&server_a.endpoint, "");
    let (records_a, _) = a.run("ctx-host-a");
    let server_b = common::ScriptedServer::start(ordinary_script());
    let b = project(&server_b.endpoint, "");
    // The same context id on both hosts: B holds a conversation record of the same name, sealed
    // by B's key, which is not the one A's trace points at.
    b.run("ctx-host-a");

    let elsewhere = TempDir::new().unwrap();
    let copy = elsewhere.path().join("copied-record");
    copy_dir(&records_a, &copy);
    let a_pub = elsewhere.path().join("a.pub");
    fs::copy(a.murmur_home().join("keys/record-seal.pub"), &a_pub).unwrap();
    let a_id = record_seal::PublicKey::load(&a_pub)
        .unwrap()
        .key_id()
        .to_string();
    let b_id = record_seal::PublicKey::load(&b.murmur_home().join("keys/record-seal.pub"))
        .unwrap()
        .key_id()
        .to_string();
    assert_ne!(a_id, b_id);

    let trace = copy.join("trace.jsonl");
    let verified = mur(b.home.path())
        .args([
            "trace",
            "verify",
            trace.to_str().unwrap(),
            "--public-key",
            a_pub.to_str().unwrap(),
        ])
        .assert()
        .success();
    let text = stdout_of(&verified);
    println!("── with A's key\n{text}");
    assert!(
        text.contains(&format!("record: verified, checked with key {a_id}")),
        "{text}"
    );
    let row = text
        .lines()
        .find(|line| line.starts_with("conversation:seal-capsule/ctx-host-a"))
        .unwrap_or_else(|| panic!("no conversation row: {text}"));
    assert!(row.contains("absent"), "{row}");

    let foreign = mur(b.home.path())
        .args(["trace", "verify", trace.to_str().unwrap()])
        .assert()
        .failure();
    let error = stderr_of(&foreign);
    println!("── with B's key\n{}{error}", stdout_of(&foreign));
    assert!(error.contains("E-TRC-005"), "{error}");
    assert!(error.contains(&format!("sealed by key {a_id}")), "{error}");
    assert!(error.contains(&b_id), "{error}");
}

/// The `capsule.session` span carries the chain head at `session_end`.
#[test]
fn session_span_carries_the_chain_head() {
    if skip("session_span_carries_the_chain_head") {
        return;
    }
    let (port, bodies) = otlp_collector();
    let server = common::ScriptedServer::start(ordinary_script());
    let p = project(
        &server.endpoint,
        &format!("observability:\n  otel_endpoint: http://127.0.0.1:{port}\n"),
    );
    let (records, _) = p.run("ctx-otel");
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let (number, end_line) = trace
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("\"event_type\":\"session_end\""))
        .expect("a session_end line");
    let seal: Value =
        serde_json::from_str(&fs::read_to_string(records.join("trace.jsonl.seal")).unwrap())
            .unwrap();
    let key_id = record_seal::PublicKey::load(&p.murmur_home().join("keys/record-seal.pub"))
        .unwrap()
        .key_id()
        .to_string();

    let spans: Vec<Value> = bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| {
            body["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let sessions: Vec<&Value> = spans
        .iter()
        .filter(|span| span["name"] == "capsule.session")
        .collect();
    assert_eq!(sessions.len(), 1, "{spans:#?}");
    let attribute = |key: &str| span_attribute(sessions[0], key).map(str::to_string);
    let head = line_hash(end_line.as_bytes());
    assert_eq!(attribute("murmur.record.head"), Some(head.clone()));
    assert_eq!(
        attribute("murmur.record.lines"),
        Some((number + 1).to_string())
    );
    assert_eq!(attribute("murmur.record.key_id"), Some(key_id));
    assert_eq!(seal["head"], head.as_str());
}

/// A truncated conversation verifies after `mur conversation truncate`, a second
/// run's appends are resealed with every earlier message byte for byte, and every write of a
/// delegated child's `completion.json` leaves a seal that verifies.
#[test]
#[allow(clippy::print_stdout)]
fn completion_and_conversation_reseal() {
    if skip("completion_and_conversation_reseal") {
        return;
    }
    let mut script = ordinary_script();
    script.extend(ordinary_script());
    let server = common::ScriptedServer::start(script);
    let p = project(&server.endpoint, "");
    let (first, _) = p.run("ctx-trunc");
    let conversation = p.conversation("ctx-trunc");
    let before = fs::read_to_string(&conversation).unwrap();
    p.verify(&first, &[]).success();

    mur(p.home.path())
        .args(["conversation", "truncate", "ctx-trunc", "--keep", "2"])
        .assert()
        .success();
    let after = fs::read_to_string(&conversation).unwrap();
    assert!(after.lines().count() < before.lines().count(), "{after}");
    for line in after.lines().filter(|line| !line.contains("murmur.record")) {
        assert!(
            before.lines().any(|kept| kept == line),
            "a kept message changed: {line}"
        );
    }
    let report = p.verify_json(&first);
    assert_eq!(
        stream(&report, "conversation:seal-capsule/ctx-trunc")["verdict"],
        "verified",
        "{report:#}"
    );

    // A second run on the same context appends and reseals; the lines it found are unchanged.
    let (second, _) = p.run("ctx-trunc");
    let grown = fs::read_to_string(&conversation).unwrap();
    assert!(
        grown.starts_with(&after),
        "earlier lines changed:\n{after}\n---\n{grown}"
    );
    p.verify(&second, &[]).success();

    // A delegated child's completion, written then rewritten after delivery.
    let home = common::process_home();
    fs::create_dir_all(home.join(".murmur")).unwrap();
    let child = TempDir::new().unwrap();
    let root = capsule_runtime::WorkdirRoot::open(child.path()).unwrap();
    let mut outcome: capsule_runtime::delegation::DelegationOutcome =
        serde_json::from_value(completion_json()).unwrap();
    capsule_runtime::delegation::write_completion(&root, &outcome).unwrap();
    outcome.delivered = true;
    capsule_runtime::delegation::write_completion(&root, &outcome).unwrap();
    let key =
        record_seal::PublicKey::load(&record_seal::public_key_path(&home.join(".murmur"))).unwrap();
    let report = record_seal::verify_session_records(child.path(), Some(&key), None);
    let completion = report
        .streams
        .iter()
        .find(|stream| stream.stream == "completion.json")
        .unwrap();
    assert_eq!(
        completion.verdict,
        record_seal::Verdict::Verified,
        "{report:#?}"
    );
    let path = child.path().join("completion.json");
    replace_in(&path, "\"delivered\": true", "\"delivered\": fals");
    let report = record_seal::verify_session_records(child.path(), Some(&key), None);
    assert_eq!(report.verdict, record_seal::Verdict::Changed, "{report:#?}");
}

fn completion_json() -> Value {
    json!({
        "delegation_id": "dlg_seal",
        "capsule_name": "child",
        "capsule_version": "0.1.0",
        "session_id": "ses_00000000000000000000000000000001",
        "status": "ok",
        "workdir": "/tmp/child",
        "duration_ms": 1,
        "reported_by": "child",
        "delivered": false
    })
}

/// A trace written before records were sealed is unverifiable and every reader
/// still reads it; a wide or linked key leaves the session running and its record unsealed;
/// a missing public half is written again; two first launches share one key.
#[test]
#[allow(clippy::print_stdout)]
fn legacy_and_key_problems() {
    if skip("legacy_and_key_problems") {
        return;
    }
    let mut script = Vec::new();
    for _ in 0..6 {
        script.extend(ordinary_script());
    }
    let server = common::ScriptedServer::start_answering(64, {
        let mut script = script.into_iter();
        move |_| script.next().unwrap_or_else(|| end_turn("done"))
    });
    let p = project(&server.endpoint, "");

    // Two first launches at once: one key, both records verify under it.
    let children: Vec<_> = ["ctx-race-1", "ctx-race-2"]
        .iter()
        .map(|context| {
            std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
                .env("HOME", p.home.path())
                .current_dir(p.project.path())
                .args(p.run_args(context))
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut raced = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        raced.push(common::parse_records_from_stdout(
            &String::from_utf8(output.stdout).unwrap(),
        ));
    }
    let keys = p.murmur_home().join("keys");
    let names: Vec<String> = fs::read_dir(&keys)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    for records in &raced {
        p.verify(records, &[]).success();
    }

    // A legacy trace: the same lines without prev_hash.
    let legacy_id = "ses_0000000000007000800000000000aaaa";
    let legacy = common::session_records_dir(p.home.path(), p.workdir.path(), legacy_id);
    fs::create_dir_all(&legacy).unwrap();
    let source = fs::read_to_string(raced[0].join("trace.jsonl")).unwrap();
    let stripped: String = source
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).unwrap();
            value.as_object_mut().unwrap().remove("prev_hash");
            value["session_id"] = json!(legacy_id);
            format!("{}\n", serde_json::to_string(&value).unwrap())
        })
        .collect();
    fs::write(legacy.join("trace.jsonl"), stripped).unwrap();
    let error = stderr_of(&p.verify(&legacy, &[]).failure());
    assert!(error.contains("E-TRC-005"), "{error}");
    assert!(
        error.contains("written before records were sealed"),
        "{error}"
    );
    let workdir = p.workdir.path().to_str().unwrap();
    let shown = stdout_of(
        &mur(p.home.path())
            .args(["trace", "show", legacy_id, "--workdir", workdir])
            .assert()
            .success(),
    );
    assert!(shown.contains("record:"), "{shown}");
    assert!(shown.contains("unverifiable"), "{shown}");
    for args in [
        vec!["trace", "steps", legacy_id, "--workdir", workdir],
        vec!["trace", "diff", legacy_id, legacy_id, "--workdir", workdir],
        vec!["trace", "report", "--workdir", workdir],
        vec!["trace", "list", "--workdir", workdir],
    ] {
        mur(p.home.path()).args(&args).assert().success();
    }

    // A missing public half beside a valid key is written again on the next launch.
    let public = fs::read_to_string(keys.join("record-seal.pub")).unwrap();
    fs::remove_file(keys.join("record-seal.pub")).unwrap();
    let (records, _) = p.run("ctx-pub");
    assert_eq!(
        fs::read_to_string(keys.join("record-seal.pub")).unwrap(),
        public
    );
    p.verify(&records, &[]).success();

    // A key readable by others: the session runs, says why once, and its record is unsealed.
    let private = keys.join("record-seal.key");
    let seed = fs::read_to_string(&private).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private, fs::Permissions::from_mode(0o644)).unwrap();
    }
    let (records, stderr) = p.run("ctx-wide");
    println!("── wide key stderr\n{stderr}");
    let reports: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("record-seal key could not be used"))
        .collect();
    assert_eq!(reports.len(), 1, "{stderr}");
    assert!(
        reports[0].contains(&private.display().to_string()),
        "{stderr}"
    );
    assert!(reports[0].contains("0644"), "{stderr}");
    assert!(!stderr.contains(seed.trim()), "the key leaked into stderr");
    let error = stderr_of(&p.verify(&records, &[]).failure());
    assert!(error.contains("E-TRC-004"), "{error}");

    // A key that is a symbolic link: the same.
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let moved = p.murmur_home().join("moved.key");
    fs::rename(&private, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &private).unwrap();
    let (records, stderr) = p.run("ctx-link");
    assert!(stderr.contains("symbolic link"), "{stderr}");
    assert!(!stderr.contains(seed.trim()), "the key leaked into stderr");
    let error = stderr_of(&p.verify(&records, &[]).failure());
    assert!(error.contains("E-TRC-004"), "{error}");
}

// ── OTLP collector ───────────────────────────────────────────────────────────

/// Every OTLP request body posted to the returned port, as it arrives; each is answered `200`.
fn otlp_collector() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use std::io::{BufRead, BufReader, Read};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0; length];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            if request_line.starts_with("POST /v1/traces ") {
                if let Ok(body) = serde_json::from_slice(&body) {
                    kept.lock().unwrap().push(body);
                }
            }
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (port, bodies)
}

fn span_attribute<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)?["value"]["stringValue"]
        .as_str()
}
