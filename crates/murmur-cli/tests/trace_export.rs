//! A sealed run exports as one bundle that verifies with a public key: `mur trace export` packs
//! one ended session's record — streams and seals, blobs, attachments and the conversation record
//! its trace seal names — with a `bundle.json` listing every file, and `mur trace verify --bundle`
//! checks it on another host with that host's public key alone.
//!
//! The model is scripted against the anthropic driver fixture. What runs is real: the `mur`
//! binary, its staging, and the host keys it creates under scratch `HOME`s.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::record_seal;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::write::{FileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "export-capsule";
const TASK: &str = "List the work folder and report.";
const SHELL_ALLOW: &[&str] = &["bash", "ls"];
/// `trace.capture: content` keeps blobs; `context.record: on` keeps the conversation record.
const FULL: &str = "trace:\n  capture: content\ncontext:\n  record: on\n";
const MANIFEST_KEYS: [&str; 8] = [
    "format",
    "session_id",
    "exported_at",
    "exported_by",
    "formats",
    "key_id",
    "conversation",
    "files",
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
}

/// A capsule on the scripted driver at `endpoint`, with `extra` appended to its manifest.
fn project(endpoint: &str, extra: &str) -> Project {
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
         driver:\n    artifact: {DRIVER_NAME}\n{extra}"
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
            "advisory".into(),
            "--verbose".into(),
        ]
    }

    /// `mur run` to the end, returning the session's records directory.
    fn run(&self, context: &str) -> PathBuf {
        let assert = mur(self.home.path())
            .current_dir(self.project.path())
            .args(self.run_args(context))
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        common::parse_records_from_stdout(&stdout)
    }

    fn murmur_home(&self) -> PathBuf {
        self.home.path().join(".murmur")
    }

    fn context_dir(&self, context: &str) -> PathBuf {
        self.murmur_home()
            .join("conversations")
            .join(CAPSULE_NAME)
            .join(context)
    }

    fn attach(&self, records: &Path) {
        mur(self.home.path())
            .args(["trace", "attach", records.to_str().unwrap()])
            .args(["--evaluator", "ci", "--value", "0.75"])
            .assert()
            .success();
    }

    fn export(&self, records: &Path, extra: &[&str]) -> Assert {
        mur(self.home.path())
            .current_dir(self.project.path())
            .args(["trace", "export", records.to_str().unwrap()])
            .args(extra)
            .assert()
    }

    fn public_key(&self) -> PathBuf {
        self.murmur_home().join("keys/record-seal.pub")
    }
}

/// A full session: one run with blobs and a conversation record, then one attachment.
fn full_session(context: &str) -> (Project, PathBuf, common::ScriptedServer) {
    let server =
        common::ScriptedServer::start(vec![tool_call(1, "toolu_ls", "ls"), end_turn("done")]);
    let p = project(&server.endpoint, FULL);
    let records = p.run(context);
    p.attach(&records);
    (p, records, server)
}

fn verify_bundle(home: &Path, bundle: &Path, extra: &[&str]) -> Assert {
    mur(home)
        .args(["trace", "verify", "--bundle", bundle.to_str().unwrap()])
        .args(extra)
        .assert()
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn json_of(assert: &Assert) -> Value {
    let stdout = &assert.get_output().stdout;
    serde_json::from_slice(stdout).unwrap_or_else(|error| {
        panic!(
            "--json printed no object ({error}): {}",
            String::from_utf8_lossy(stdout)
        )
    })
}

fn skip(test: &str) -> bool {
    common::skip_without_host_support(test)
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn sha256_hex(bytes: &[u8]) -> String {
    murmur_artifact::sha256_hex(bytes)
}

/// Every file beneath `dir`, by its `/`-separated path relative to `dir`, with its bytes.
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, found: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(base, &path, found);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let bytes = if kind.is_symlink() {
                    format!("-> {}", fs::read_link(&path).unwrap().display()).into_bytes()
                } else {
                    fs::read(&path).unwrap()
                };
                found.insert(rel, bytes);
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(dir, dir, &mut found);
    found
}

/// Every directory beneath `dir`, relative to it.
fn dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        if entry.file_type().unwrap().is_dir() {
            found.push(entry.path());
            found.extend(dirs(&entry.path()));
        }
    }
    found
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

/// What a bundle of `records` must hold: the bundle path of every file that should be packed,
/// mapped to its source.
fn expected_sources(p: &Project, records: &Path, context: &str) -> BTreeMap<String, PathBuf> {
    let mut expected = BTreeMap::new();
    for name in ["trace.jsonl", "trace.jsonl.seal"] {
        expected.insert(name.to_string(), records.join(name));
    }
    for dir in ["blobs", "attachments"] {
        for (rel, _) in tree(&records.join(dir)) {
            expected.insert(format!("{dir}/{rel}"), records.join(dir).join(&rel));
        }
    }
    let context_dir = p.context_dir(context);
    let prefix = format!("conversations/{CAPSULE_NAME}/{context}");
    for rel in ["conversation.jsonl", "conversation.jsonl.seal"] {
        expected.insert(format!("{prefix}/{rel}"), context_dir.join(rel));
    }
    for (rel, _) in tree(&context_dir.join("blobs")) {
        expected.insert(
            format!("{prefix}/blobs/{rel}"),
            context_dir.join("blobs").join(rel),
        );
    }
    expected
}

fn zip_entries(path: &Path) -> BTreeMap<String, (Vec<u8>, Option<u32>)> {
    let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let mut found = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        found.insert(name, (bytes, entry.unix_mode()));
    }
    found
}

/// Writes a zip at `path` holding `entries` in order, `None` content meaning a symbolic link to
/// `/etc/passwd`.
fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
    let mut zip = ZipWriter::new(fs::File::create(path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in entries {
        match bytes {
            Some(bytes) => {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            None => zip.add_symlink(*name, "/etc/passwd", options).unwrap(),
        }
    }
    zip.finish().unwrap();
}

/// Rewrites the zip at `path` with `edit` applied to every entry's bytes.
fn rewrite_zip(path: &Path, edit: impl Fn(&str, &mut Vec<u8>)) {
    let entries = zip_entries(path);
    let mut zip = ZipWriter::new(fs::File::create(path).unwrap());
    let options: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, (mut bytes, _)) in entries {
        edit(&name, &mut bytes);
        zip.start_file(name, options).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn flip_first_byte(bytes: &mut [u8]) {
    bytes[0] ^= 0x01;
}

/// The first blob of a bundle at `bundle`, by name.
fn first_blob(bundle: &Path) -> String {
    let mut names: Vec<String> = fs::read_dir(bundle.join("blobs"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().next().expect("a blob")
}

/// Replaces the first `"event_id":"evt_<hex>` hex digit on `line` (1-based) of `path` with
/// another hex digit: one byte inside a string value.
fn flip_event_id_byte(path: &Path, line: usize) {
    let text = fs::read_to_string(path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let target = &mut lines[line - 1];
    let at = target.find("\"event_id\":\"evt_").expect("an event id") + "\"event_id\":\"evt_".len();
    let replacement = if target.as_bytes()[at] == b'a' {
        "b"
    } else {
        "a"
    };
    target.replace_range(at..=at, replacement);
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Rewrites `bundle.json` in `bundle` with `edit` applied.
fn edit_manifest(bundle: &Path, edit: impl FnOnce(&mut Value)) {
    let path = bundle.join("bundle.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    fs::write(
        &path,
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .unwrap();
}

/// Brings `files[]` of `bundle.json` in `bundle` in line with what the bundle holds.
fn relist(bundle: &Path) {
    edit_manifest(bundle, |manifest| {
        let files: Vec<Value> = tree(bundle)
            .into_iter()
            .filter(|(path, _)| path != "bundle.json")
            .map(|(path, bytes)| json!({"path": path, "bytes": bytes.len(), "sha256": sha256_hex(&bytes)}))
            .collect();
        manifest["files"] = Value::Array(files);
    });
}

fn stream<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["stream"] == name)
        .unwrap_or_else(|| panic!("no {name} stream in {report:#}"))
}

// ── scenarios ────────────────────────────────────────────────────────────────

/// A directory export holds exactly the record's sealed files, byte for byte, at private
/// modes, with a manifest naming each one, and leaves the sources as they were.
#[test]
#[allow(clippy::print_stdout)]
fn s1_directory_export_holds_exactly_the_sealed_record() {
    if skip("s1_directory_export_holds_exactly_the_sealed_record") {
        return;
    }
    let context = "ctx-s1";
    let (p, records, _server) = full_session(context);
    let records_before = tree(&records);
    let conversation_before = tree(&p.context_dir(context));
    let session_id = records.file_name().unwrap().to_string_lossy().into_owned();
    let out = p.project.path().join("bundle-dir");

    let exported = p
        .export(&records, &["--out", out.to_str().unwrap()])
        .success();
    let text = stdout_of(&exported);
    println!("── export\n{text}");

    let expected = expected_sources(&p, &records, context);
    assert!(
        expected.keys().any(|path| path.starts_with("blobs/")),
        "capture: content keeps at least one blob: {expected:#?}"
    );
    assert!(expected
        .keys()
        .any(|path| path.starts_with("attachments/att_")));
    let held = tree(&out);
    let mut paths: Vec<&String> = held.keys().collect();
    paths.retain(|path| *path != "bundle.json");
    assert_eq!(
        paths,
        expected.keys().collect::<Vec<_>>(),
        "the bundle holds exactly the sealed record"
    );
    for (path, source) in &expected {
        assert_eq!(
            &held[path],
            &fs::read(source).unwrap(),
            "{path} is copied byte for byte"
        );
    }
    for name in ["logs", "out", "tmp", "home", "children"] {
        assert!(!out.join(name).exists(), "{name}/ is not a record");
    }
    assert_eq!(mode(&out), 0o700);
    for dir in dirs(&out) {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    for path in held.keys() {
        assert_eq!(mode(&out.join(path)), 0o600, "{path}");
    }

    let manifest_text = fs::read_to_string(out.join("bundle.json")).unwrap();
    assert!(manifest_text.ends_with("}\n"));
    let positions: Vec<usize> = MANIFEST_KEYS
        .iter()
        .map(|key| manifest_text.find(&format!("\n  \"{key}\":")).unwrap())
        .collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(
        positions, sorted,
        "bundle.json keys in order: {manifest_text}"
    );
    let manifest: Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest.as_object().unwrap().len(), MANIFEST_KEYS.len());
    assert_eq!(manifest["format"], "murmur.bundle/1");
    assert_eq!(manifest["session_id"], session_id.as_str());
    assert!(manifest["exported_at"].as_u64().unwrap() > 1_700_000_000_000);
    assert!(manifest["exported_by"]
        .as_str()
        .unwrap()
        .starts_with("mur "));
    assert_eq!(
        manifest["formats"],
        json!({"seal": "murmur.seal/1", "attachment": "murmur.attachment/1"})
    );
    let key_id = record_seal::PublicKey::load(&p.public_key())
        .unwrap()
        .key_id()
        .to_string();
    assert_eq!(manifest["key_id"], key_id.as_str());
    assert_eq!(
        manifest["conversation"],
        json!({"record": CAPSULE_NAME, "context_id": context})
    );
    let files = manifest["files"].as_array().unwrap();
    let listed: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(listed, paths.iter().map(|p| p.as_str()).collect::<Vec<_>>());
    let mut total = 0;
    for file in files {
        let bytes = &held[file["path"].as_str().unwrap()];
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(file["sha256"], sha256_hex(bytes).as_str());
        total += bytes.len();
    }

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            format!("exported session {session_id} to {}", out.display()).as_str(),
            format!("{} files, {total} bytes", files.len()).as_str(),
            "record: verified",
        ]
    );
    assert_eq!(
        tree(&records),
        records_before,
        "the records directory is unchanged"
    );
    assert_eq!(tree(&p.context_dir(context)), conversation_before);
}

/// A zip export holds the directory export's tree, entry for entry, at mode 0600; `--json` prints
/// one object; with no `--out` the zip lands in the current directory named for the session.
#[test]
#[allow(clippy::print_stdout)]
fn s2_zip_export_holds_the_same_tree() {
    if skip("s2_zip_export_holds_the_same_tree") {
        return;
    }
    let (p, records, _server) = full_session("ctx-s2");
    let session_id = records.file_name().unwrap().to_string_lossy().into_owned();
    let dir = p.project.path().join("bundle-dir");
    p.export(&records, &["--out", dir.to_str().unwrap()])
        .success();
    let zip_path = p.project.path().join("bundle.ZIP");
    let exported = p
        .export(&records, &["--out", zip_path.to_str().unwrap(), "--json"])
        .success();
    let output = json_of(&exported);
    println!("── export --json\n{output:#}");

    let held = tree(&dir);
    let entries = zip_entries(&zip_path);
    assert_eq!(
        entries.keys().collect::<Vec<_>>(),
        held.keys().collect::<Vec<_>>()
    );
    for (name, (bytes, unix_mode)) in &entries {
        if name != "bundle.json" {
            assert_eq!(bytes, &held[name], "{name}");
        }
        assert_eq!(unix_mode.map(|m| m & 0o777), Some(0o600), "{name}");
    }
    let from_zip: Value = serde_json::from_slice(&entries["bundle.json"].0).unwrap();
    let from_dir: Value = serde_json::from_slice(&held["bundle.json"]).unwrap();
    assert_eq!(from_zip["files"], from_dir["files"]);
    assert_eq!(mode(&zip_path), 0o600);
    let total: u64 = from_zip["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(
        output,
        json!({
            "session_id": session_id,
            "path": zip_path.display().to_string(),
            "form": "zip",
            "files": from_zip["files"].as_array().unwrap().len(),
            "bytes": total,
            "verdict": "verified",
        })
    );

    p.export(&records, &[]).success();
    let default = p.project.path().join(format!("{session_id}.bundle.zip"));
    assert!(default.is_file(), "{}", default.display());
    assert_eq!(
        zip_entries(&default).keys().collect::<Vec<_>>(),
        entries.keys().collect::<Vec<_>>()
    );
}

/// On another host, with the exporting host's public key and nothing else, both bundle forms
/// verify; without that key the record names the key that sealed it.
#[test]
#[allow(clippy::print_stdout)]
fn s3_bundle_verifies_on_another_machine() {
    if skip("s3_bundle_verifies_on_another_machine") {
        return;
    }
    let context = "ctx-s3";
    let (p, records, _server) = full_session(context);
    let elsewhere = TempDir::new().unwrap();
    let dir = elsewhere.path().join("bundle-dir");
    let zip_path = elsewhere.path().join("bundle.zip");
    p.export(&records, &["--out", dir.to_str().unwrap()])
        .success();
    p.export(&records, &["--out", zip_path.to_str().unwrap()])
        .success();
    let a_pub = elsewhere.path().join("a.pub");
    fs::copy(p.public_key(), &a_pub).unwrap();
    let a_id = record_seal::PublicKey::load(&a_pub)
        .unwrap()
        .key_id()
        .to_string();
    let moved = TempDir::new().unwrap();
    let moved_home = moved.path().join("a-home");
    fs::rename(p.home.path(), &moved_home).unwrap();

    let host_b = TempDir::new().unwrap();
    let b_murmur = host_b.path().join(".murmur");
    let b_id = record_seal::HostKey::load_or_create(&b_murmur)
        .unwrap()
        .key_id()
        .to_string();
    assert_ne!(a_id, b_id);
    let session_id = records.file_name().unwrap().to_string_lossy().into_owned();
    let conversation = format!("conversation:{CAPSULE_NAME}/{context}");

    for bundle in [&dir, &zip_path] {
        let verified = verify_bundle(
            host_b.path(),
            bundle,
            &["--public-key", a_pub.to_str().unwrap()],
        )
        .success();
        let text = stdout_of(&verified);
        println!("── verify {}\n{text}", bundle.display());
        for name in ["trace.jsonl", conversation.as_str()] {
            let row = text
                .lines()
                .find(|line| line.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} row: {text}"));
            assert!(row.contains("verified"), "{row}");
        }
        assert!(
            text.contains(&format!("record: verified, checked with key {a_id}")),
            "{text}"
        );
        assert!(text.contains("matches"), "{text}");
        assert!(text.contains("bundle: verified, "), "{text}");

        let verified_json = verify_bundle(
            host_b.path(),
            bundle,
            &["--public-key", a_pub.to_str().unwrap(), "--json"],
        )
        .success();
        let raw = stdout_of(&verified_json);
        let report = json_of(&verified_json);
        assert_eq!(report["schema"], 1);
        assert_eq!(report["verdict"], "verified");
        assert_eq!(report["session_id"], session_id.as_str());
        assert_eq!(report["records_dir"], bundle.display().to_string());
        assert_eq!(stream(&report, "trace.jsonl")["verdict"], "verified");
        assert_eq!(stream(&report, &conversation)["verdict"], "verified");
        assert_eq!(report["attachments"][0]["status"], "matches");
        let bundle_report = &report["bundle"];
        let bundle_keys = [
            "path",
            "format",
            "session_id",
            "exported_at",
            "verdict",
            "files",
            "findings",
            "notes",
        ];
        assert_eq!(bundle_report.as_object().unwrap().len(), bundle_keys.len());
        let block = &raw[raw.find("\n  \"bundle\": {").expect("a bundle key")..];
        let positions: Vec<usize> = bundle_keys
            .iter()
            .map(|key| block.find(&format!("\n    \"{key}\":")).unwrap())
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "bundle keys in order: {block}");
        assert_eq!(bundle_report["path"], bundle.display().to_string());
        assert_eq!(bundle_report["format"], "murmur.bundle/1");
        assert_eq!(bundle_report["session_id"], session_id.as_str());
        assert_eq!(bundle_report["verdict"], "verified");
        assert_eq!(bundle_report["findings"], json!([]));
        assert_eq!(bundle_report["notes"], json!([]));
        assert!(bundle_report["files"].as_u64().unwrap() >= 6);

        let foreign = verify_bundle(host_b.path(), bundle, &[]).failure();
        let error = stderr_of(&foreign);
        println!("── with B's key\n{error}");
        assert!(error.contains("E-TRC-005"), "{error}");
        assert!(
            error.contains(&format!(
                "sealed by key {a_id}, not by the key given ({b_id})"
            )),
            "{error}"
        );
    }
    fs::rename(&moved_home, p.home.path()).unwrap();
}

/// One changed byte of a blob fails verify naming the blob, in either form; a forged blob
/// saved under its own hash with the manifest rewritten verifies with a note.
#[test]
#[allow(clippy::print_stdout)]
fn s4_a_changed_blob_is_named() {
    if skip("s4_a_changed_blob_is_named") {
        return;
    }
    let (p, records, _server) = full_session("ctx-s4");
    let session_id = records.file_name().unwrap().to_string_lossy().into_owned();
    let dir = p.project.path().join("bundle-dir");
    let zip_path = p.project.path().join("bundle.zip");
    p.export(&records, &["--out", dir.to_str().unwrap()])
        .success();
    p.export(&records, &["--out", zip_path.to_str().unwrap()])
        .success();
    let home = p.home.path();
    let blob = first_blob(&dir);
    let blob_path = dir.join("blobs").join(&blob);
    let original = fs::read(&blob_path).unwrap();
    let mut edited = original.clone();
    flip_first_byte(&mut edited);
    let actual = sha256_hex(&edited);

    fs::write(&blob_path, &edited).unwrap();
    let failed = verify_bundle(home, &dir, &[]).failure();
    let error = stderr_of(&failed);
    let text = stdout_of(&failed);
    println!("── changed blob\n{text}{error}");
    assert!(
        error.contains(&format!(
            "error[E-TRC-003]: the bundle of session {session_id} has changed: blobs/{blob}: \
             holds bytes whose sha256 is {actual}, not the {blob} it is named by"
        )),
        "{error}"
    );
    let trace_row = text.lines().find(|l| l.starts_with("trace.jsonl")).unwrap();
    assert!(trace_row.contains("verified"), "{trace_row}");
    assert!(text.contains("bundle: changed"), "{text}");

    let blob_entry = format!("blobs/{blob}");
    rewrite_zip(&zip_path, |name, bytes| {
        if name == blob_entry {
            flip_first_byte(bytes);
        }
    });
    let error = stderr_of(&verify_bundle(home, &zip_path, &[]).failure());
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(
        error.contains(&format!(
            "blobs/{blob}: holds bytes whose sha256 is {actual}"
        )),
        "{error}"
    );

    fs::remove_file(&blob_path).unwrap();
    fs::write(dir.join("blobs").join(&actual), &edited).unwrap();
    relist(&dir);
    let forged = verify_bundle(home, &dir, &[]).success();
    let text = stdout_of(&forged);
    println!("── forged blob\n{text}");
    assert!(
        text.contains(&format!(
            "note: blobs/{actual} is not named by any sealed line of the record"
        )),
        "{text}"
    );

    fs::remove_file(dir.join("blobs").join(&actual)).unwrap();
    fs::write(&blob_path, &original).unwrap();
    relist(&dir);
    verify_bundle(home, &dir, &[]).success();
}

/// A changed trace line, attachment, deleted file or session id fails verify naming it; an
/// unlisted extra file is a note.
#[test]
#[allow(clippy::print_stdout)]
fn s5_changed_files_and_manifest_are_named() {
    if skip("s5_changed_files_and_manifest_are_named") {
        return;
    }
    let (p, records, _server) = full_session("ctx-s5");
    let session_id = records.file_name().unwrap().to_string_lossy().into_owned();
    let pristine = p.project.path().join("pristine");
    p.export(&records, &["--out", pristine.to_str().unwrap()])
        .success();
    let home = p.home.path();
    let fresh = |name: &str| {
        let copy = p.project.path().join(name);
        copy_dir(&pristine, &copy);
        copy
    };

    let trace = fresh("trace-edit");
    flip_event_id_byte(&trace.join("trace.jsonl"), 3);
    let error = stderr_of(&verify_bundle(home, &trace, &[]).failure());
    println!("── trace line 3\n{error}");
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(error.contains("trace.jsonl line 3"), "{error}");

    let attachment = fresh("attachment-edit");
    let att_dir = attachment.join("attachments");
    let att_name = fs::read_dir(&att_dir)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let att_path = att_dir.join(&att_name);
    let before = fs::read(&att_path).unwrap();
    let text = String::from_utf8(before.clone()).unwrap();
    assert!(text.contains("\"0.75\""), "{text}");
    fs::write(&att_path, text.replacen("\"0.75\"", "\"0.99\"", 1)).unwrap();
    let after = fs::read(&att_path).unwrap();
    let error = stderr_of(&verify_bundle(home, &attachment, &[]).failure());
    println!("── attachment value\n{error}");
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(
        error.contains(&format!(
            "the bundle of session {session_id} has changed: bundle.json lists sha256 {} for \
             attachments/{att_name}; the file holds {}",
            sha256_hex(&before),
            sha256_hex(&after)
        )),
        "{error}"
    );

    let deleted = fresh("deleted");
    fs::remove_file(deleted.join("attachments").join(&att_name)).unwrap();
    let error = stderr_of(&verify_bundle(home, &deleted, &[]).failure());
    println!("── deleted\n{error}");
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(
        error.contains(&format!(
            "bundle.json lists attachments/{att_name} ({} bytes, sha256 {}), which the bundle \
             does not hold",
            before.len(),
            sha256_hex(&before)
        )),
        "{error}"
    );

    let extra = fresh("extra");
    fs::write(extra.join("notes.txt"), "an unlisted file\n").unwrap();
    let text = stdout_of(&verify_bundle(home, &extra, &[]).success());
    println!("── extra\n{text}");
    assert!(
        text.contains("note: notes.txt is not listed in bundle.json"),
        "{text}"
    );

    let renamed = fresh("session-edit");
    edit_manifest(&renamed, |manifest| {
        manifest["session_id"] = json!("ses_00000000000000000000000000000000");
    });
    let error = stderr_of(&verify_bundle(home, &renamed, &[]).failure());
    println!("── session id\n{error}");
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(
        error.contains(&format!(
            "bundle.json names session ses_00000000000000000000000000000000; trace.jsonl is \
             session {session_id}"
        )),
        "{error}"
    );
}

/// A path that is not a usable bundle is refused with E-TRC-009 before any report, and a zip
/// that would escape or plant a link writes nothing anywhere.
#[test]
fn s6_unusable_bundles_are_refused() {
    let home = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let tmp = scratch.path().join("tmp");
    fs::create_dir(&tmp).unwrap();
    let base = scratch.path().join("cases");
    fs::create_dir(&base).unwrap();
    let trace: &[u8] = b"{\"session_id\":\"ses_x\",\"prev_hash\":\"genesis\"}\n";
    let good_manifest: &[u8] = br#"{"format":"murmur.bundle/1","session_id":null,"exported_at":1,"exported_by":"mur","formats":{"seal":"murmur.seal/1","attachment":"murmur.attachment/1"},"key_id":null,"conversation":null,"files":[]}"#;

    let empty = base.join("empty");
    fs::create_dir(&empty).unwrap();
    let wrong_format = base.join("wrong-format");
    fs::create_dir(&wrong_format).unwrap();
    fs::write(
        wrong_format.join("bundle.json"),
        r#"{"format":"murmur.bundle/9"}"#,
    )
    .unwrap();
    let not_json = base.join("not-json");
    fs::create_dir(&not_json).unwrap();
    fs::write(not_json.join("bundle.json"), "not json at all").unwrap();
    let escape = base.join("escape.zip");
    write_zip(
        &escape,
        &[
            ("bundle.json", Some(good_manifest)),
            ("../escape.txt", Some(b"escaped")),
        ],
    );
    let link = base.join("link.zip");
    write_zip(
        &link,
        &[("bundle.json", Some(good_manifest)), ("trace.jsonl", None)],
    );
    let duplicate = base.join("duplicate.zip");
    write_zip(
        &duplicate,
        &[
            ("bundle.json", Some(good_manifest)),
            ("trace.jsonl", Some(trace)),
            ("./trace.jsonl", Some(trace)),
        ],
    );
    let missing = base.join("does-not-exist");

    let cases: [(&Path, &str); 7] = [
        (&empty, "it holds no bundle.json"),
        (
            &wrong_format,
            "has format \"murmur.bundle/9\", not murmur.bundle/1",
        ),
        (&not_json, "bundle.json is not JSON"),
        (&escape, "'..'"),
        (&link, "symbolic link"),
        (&duplicate, "two entries named trace.jsonl"),
        (&missing, "it does not exist"),
    ];
    for (path, reason) in cases {
        let assert = mur(home.path())
            .env("TMPDIR", &tmp)
            .args(["trace", "verify", "--bundle", path.to_str().unwrap()])
            .assert()
            .failure();
        let error = stderr_of(&assert);
        assert!(
            error.contains(&format!(
                "error[E-TRC-009]: {} is not a usable bundle: ",
                path.display()
            )),
            "{error}"
        );
        assert!(error.contains(reason), "{reason}: {error}");
        assert_eq!(stdout_of(&assert), "", "no report for {}", path.display());
        assert_eq!(
            fs::read_dir(&tmp).unwrap().count(),
            0,
            "no extraction directory is left behind"
        );
    }
    let escaped: Vec<String> = tree(scratch.path())
        .into_keys()
        .filter(|path| path.ends_with("escape.txt"))
        .collect();
    assert!(escaped.is_empty(), "{escaped:?}");
    assert!(!scratch.path().join("escape.txt").exists());

    for extra in [vec!["@1"], vec!["--workdir", "w"]] {
        mur(home.path())
            .args(["trace", "verify", "--bundle", empty.to_str().unwrap()])
            .args(extra)
            .assert()
            .code(2);
    }
}

/// Export refuses an existing destination, a running session, an unknown address, a link
/// among the blobs and an unwritable destination, each leaving everything as it was.
#[test]
#[allow(clippy::print_stdout)]
fn s7_export_refusals_leave_everything_untouched() {
    if skip("s7_export_refusals_leave_everything_untouched") {
        return;
    }
    let (p, records, _server) = full_session("ctx-s7");
    let outs = p.project.path().join("outs");
    fs::create_dir(&outs).unwrap();
    let existing_dir = outs.join("existing-dir");
    fs::create_dir(&existing_dir).unwrap();
    let existing_file = outs.join("existing.zip");
    fs::write(&existing_file, "").unwrap();

    let check = |out: &Path, code: &str, says: &str| {
        let records_before = tree(&records);
        let outs_before = tree(&outs);
        let dirs_before = dirs(&outs);
        let assert = p
            .export(&records, &["--out", out.to_str().unwrap()])
            .failure();
        let error = stderr_of(&assert);
        println!("── {}\n{error}", out.display());
        assert!(error.contains(code), "{error}");
        assert!(error.contains(says), "{says}: {error}");
        assert_eq!(tree(&records), records_before);
        assert_eq!(tree(&outs), outs_before);
        assert_eq!(dirs(&outs), dirs_before);
    };
    check(
        &existing_dir,
        "E-TRC-008",
        &format!(
            "{} already exists; nothing was exported",
            existing_dir.display()
        ),
    );
    check(
        &existing_file,
        "E-TRC-008",
        &format!(
            "{} already exists; nothing was exported",
            existing_file.display()
        ),
    );

    let read_only = outs.join("read-only");
    fs::create_dir(&read_only).unwrap();
    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o555)).unwrap();
    for name in ["bundle", "bundle.zip"] {
        let out = read_only.join(name);
        check(
            &out,
            "E-TRC-008",
            &format!("the bundle {} cannot be written: ", out.display()),
        );
    }
    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o755)).unwrap();

    let blobs = records.join("blobs");
    let blob = first_blob(&records);
    let original = fs::read(blobs.join(&blob)).unwrap();
    fs::remove_file(blobs.join(&blob)).unwrap();
    std::os::unix::fs::symlink(records.join("trace.jsonl"), blobs.join(&blob)).unwrap();
    check(
        &outs.join("linked"),
        "E-TRC-008",
        &format!(
            "blobs/{blob} in {} is a symbolic link; nothing was exported",
            records.display()
        ),
    );
    fs::remove_file(blobs.join(&blob)).unwrap();
    fs::write(blobs.join(&blob), original).unwrap();

    let unknown = p
        .export(
            Path::new("ffffffffffffzzzz"),
            &["--out", outs.join("x").to_str().unwrap()],
        )
        .failure();
    assert!(
        stderr_of(&unknown).contains("E-TRC-002"),
        "{}",
        stderr_of(&unknown)
    );
    assert!(!outs.join("x").exists());

    // A session still running: the second model request is held until the export has been tried.
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
    let live = project(&server.endpoint, FULL);
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", live.home.path())
        .current_dir(live.project.path())
        .args(live.run_args("ctx-live"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    arrived_rx
        .recv_timeout(std::time::Duration::from_secs(240))
        .expect("the second request arrives");
    let live_records = common::newest_session_records(live.home.path(), live.workdir.path())
        .expect("the running session has records");
    let session_id = live_records
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let out = live.project.path().join("live-bundle");
    let refused = live
        .export(&live_records, &["--out", out.to_str().unwrap()])
        .failure();
    let _ = release_tx.send(());
    let status = child.wait().unwrap();
    let error = stderr_of(&refused);
    println!("── running\n{error}");
    assert!(error.contains("E-RUN-024"), "{error}");
    assert!(
        error.contains(&format!("session {session_id} is still running (pid ")),
        "{error}"
    );
    assert!(error.contains("nothing was exported"), "{error}");
    assert!(!out.exists());
    assert!(status.success(), "{status:?}");
}

/// A changed record still exports, says so, and its bundle fails verify; a session with no
/// blobs, conversation record or attachment exports only its trace and seal.
#[test]
#[allow(clippy::print_stdout)]
fn s8_changed_and_minimal_records_export() {
    if skip("s8_changed_and_minimal_records_export") {
        return;
    }
    let (p, records, _server) = full_session("ctx-s8");
    flip_event_id_byte(&records.join("trace.jsonl"), 3);
    let out = p.project.path().join("changed");
    let exported = p
        .export(&records, &["--out", out.to_str().unwrap()])
        .success();
    let text = stdout_of(&exported);
    println!("── changed export\n{text}");
    let last = text.lines().last().unwrap();
    assert!(
        last.starts_with("record: changed: trace.jsonl line 3: "),
        "{last}"
    );
    let error = stderr_of(&verify_bundle(p.home.path(), &out, &[]).failure());
    assert!(error.contains("E-TRC-003"), "{error}");
    assert!(error.contains("trace.jsonl line 3"), "{error}");

    let server =
        common::ScriptedServer::start(vec![tool_call(1, "toolu_ls", "ls"), end_turn("done")]);
    let minimal = project(&server.endpoint, "context:\n  record: off\n");
    let records = minimal.run("ctx-min");
    let out = minimal.project.path().join("minimal");
    minimal
        .export(&records, &["--out", out.to_str().unwrap()])
        .success();
    assert_eq!(
        tree(&out).into_keys().collect::<Vec<_>>(),
        ["bundle.json", "trace.jsonl", "trace.jsonl.seal"]
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(out.join("bundle.json")).unwrap()).unwrap();
    assert_eq!(manifest["conversation"], Value::Null);
    let text = stdout_of(&verify_bundle(minimal.home.path(), &out, &[]).success());
    assert!(text.contains("bundle: verified, 2 files listed"), "{text}");
}

/// The outside checker, written from `run-bundle.md` alone with the Python standard library
/// and `openssl`, accepts an exported bundle and names a changed blob.
#[test]
#[allow(clippy::print_stdout)]
fn s9_outside_checker_follows_the_reference_page() {
    if skip("s9_outside_checker_follows_the_reference_page") {
        return;
    }
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output();
    if !python.is_ok_and(|output| output.status.success()) {
        println!("[SKIP] s9: python3 is not installed");
        return;
    }
    let openssl = std::process::Command::new("openssl")
        .arg("version")
        .output();
    match openssl {
        Ok(output) if String::from_utf8_lossy(&output.stdout).starts_with("OpenSSL 3.") => {}
        _ => {
            println!("[SKIP] s9: openssl version is not 3.x");
            return;
        }
    }
    let (p, records, _server) = full_session("ctx-s9");
    let out = p.project.path().join("bundle-dir");
    p.export(&records, &["--out", out.to_str().unwrap()])
        .success();
    let checker = common::fixture_path("bundle/check_bundle.py");
    let run = |bundle: &Path| {
        std::process::Command::new("python3")
            .arg("-I")
            .arg(&checker)
            .arg(bundle)
            .arg(p.public_key())
            .output()
            .unwrap()
    };
    let clean = run(&out);
    let text = String::from_utf8_lossy(&clean.stdout).into_owned()
        + &String::from_utf8_lossy(&clean.stderr);
    println!("── checker on the bundle\n{text}");
    assert_eq!(clean.status.code(), Some(0), "{text}");

    let edited = p.project.path().join("edited");
    copy_dir(&out, &edited);
    let blob = first_blob(&edited);
    let path = edited.join("blobs").join(&blob);
    let mut bytes = fs::read(&path).unwrap();
    flip_first_byte(&mut bytes);
    fs::write(&path, bytes).unwrap();
    let failed = run(&edited);
    let text = String::from_utf8_lossy(&failed.stdout).into_owned()
        + &String::from_utf8_lossy(&failed.stderr);
    println!("── checker on a changed blob\n{text}");
    assert_eq!(failed.status.code(), Some(1), "{text}");
    assert!(text.contains(&format!("blobs/{blob}")), "{text}");
}
