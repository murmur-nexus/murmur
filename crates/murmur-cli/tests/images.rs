//! A tool returns an image: a file it wrote into the work folder, named in its result's
//! `attachments`. The runtime checks it, stores it once by hash beside the conversation, records a
//! reference, and hands the driver the bytes as a base64 `image` block.
//!
//! The model is the `image-probe-driver` fixture: its task line scripts the tool calls it asks for,
//! and its final text reports every block of the last request it was handed, with the sha256 of
//! each image's decoded bytes. What runs is real: the staged session, the WASM `image-tool` and the
//! native fixture tool, the conversation record, the trace, the door and its stream.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use base64::Engine as _;
use capsule_runtime::launch_session;
use common::door_capsule::{bearer, launch_door, request, rpc, stage_door_with_tools, A2A_VERSION};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/image-probe-driver";
const TOOL: &str = "local/image-tool";
const NATIVE_TOOL: &str = "local/tool-fixture";
const VERSION: &str = "0.1.0";
const ENDPOINT: &str = "http://127.0.0.1:9";

fn sha256_hex(bytes: &[u8]) -> String {
    murmur_artifact::sha256_hex(bytes)
}

/// A valid 1x1 PNG, the bytes the A2A tests send.
const PNG_1X1: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// The capsule's manifest: the probe driver, the WASM image tool, and the native fixture tool when
/// `native` is set, with `context_extra` under `context:` beside its own record store.
fn manifest(name: &str, native: bool, context_extra: &str, extra: &str) -> String {
    let native_entry = if native {
        format!("  - name: {NATIVE_TOOL}\n    version: {VERSION}\n    runtime: tool\n")
    } else {
        String::new()
    };
    format!(
        "name: {name}\nversion: 0.1.0\n\
         artifacts:\n  - name: {DRIVER}\n    version: {VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {ENDPOINT}\n      api_key: test-key\n  - name: {TOOL}\n    \
         version: {VERSION}\n    runtime: tool\n{native_entry}\
         capabilities:\n  network:\n    allow:\n      - {ENDPOINT}\n\
         inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n\
         context:\n  record_store: {name}\n{context_extra}{extra}"
    )
}

/// A scratch home with the probe driver, the image tool and, when it can be built, the native
/// fixture tool published into it. `None` for `native` when the native tool cannot be built.
fn publish_fixtures(native: bool) -> Option<(TempDir, TempDir)> {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER,
        VERSION,
        &common::fixture_path("image-probe-driver/tool/image-probe-driver.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let tool = common::create_tool_artifact(
        artifacts.path(),
        TOOL,
        VERSION,
        &common::fixture_path("image-tool/tool/image-tool.wasm"),
    );
    common::publish_local(&home, &tool).success();
    if native {
        let binary = common::fixture_native_tool_binary()?;
        let manifest_bytes = fs::read(common::fixture_native_tool_manifest()).unwrap();
        let zip = common::create_native_tool_zip(
            artifacts.path(),
            common::FIXTURE_NATIVE_TOOL_NAME,
            VERSION,
            &manifest_bytes,
            &binary,
        );
        common::publish_local(&home, &zip).success();
    }
    Some((home, artifacts))
}

/// One launched session and what it left behind.
struct Run {
    /// The probe's report: one line per block of the last request it was handed.
    report: String,
    trace: Vec<Value>,
    records_dir: PathBuf,
    workdir: PathBuf,
    /// The conversation's context directory under `$HOME/.murmur/conversations/<name>/`.
    context_dir: PathBuf,
    _dirs: (TempDir, TempDir, TempDir),
}

impl Run {
    fn record(&self) -> String {
        fs::read_to_string(self.context_dir.join("conversation.jsonl")).unwrap()
    }

    fn blob_dir(&self) -> PathBuf {
        self.context_dir.join("blobs")
    }

    fn blob_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.blob_dir())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn tool_calls(&self) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|event| event["event_type"] == "tool_call")
            .collect()
    }

    /// The report's lines for image blocks with a base64 source: `(role, media_type, sha256)`.
    fn shown_images(&self) -> Vec<(String, String, String)> {
        self.report
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.splitn(7, ' ').collect();
                (fields.len() == 7 && fields[2] == "image" && fields[3] == "base64").then(|| {
                    (
                        fields[1].to_string(),
                        fields[4].to_string(),
                        fields[5].to_string(),
                    )
                })
            })
            .collect()
    }
}

/// Stages and runs one session of the capsule named `name`, whose probe asks for `turns` of
/// `tool`.
fn run(name: &str, native: bool, context_extra: &str, tool: &str, turns: Value) -> Option<Run> {
    let (home, artifacts) = publish_fixtures(native)?;
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest(name, native, context_extra, "")).unwrap();
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.accessible_workdir.clone();
    let records_dir = staged.records_dir.clone();
    fs::write(workdir.join("task.md"), format!("PROBE {tool} {turns}\n")).unwrap();
    if let Err(error) = launch_session(staged, |_| {}) {
        panic!("the session must launch: {error}");
    }
    let report = fs::read_to_string(records_dir.join("out/result.txt")).unwrap_or_default();
    println!("probe report:\n{report}");
    let trace = common::read_whole_trace(&records_dir.join("trace.jsonl"));
    let home_dir = PathBuf::from(std::env::var_os("HOME").expect("staging set a scratch HOME"));
    let store = home_dir.join(".murmur/conversations").join(name);
    let context_dir = fs::read_dir(&store)
        .unwrap_or_else(|e| panic!("{}: {e}", store.display()))
        .map(|entry| entry.unwrap().path())
        .find(|path| path.join("conversation.jsonl").is_file())
        .expect("the session recorded its conversation");
    Some(Run {
        report,
        trace,
        records_dir,
        workdir,
        context_dir,
        _dirs: (home, artifacts, project),
    })
}

/// The three things every accepted image leaves: the driver got its bytes, the record holds its
/// reference and the blob, and the trace names it, with no base64 in either file.
fn assert_attached(run: &Run, file: &str) {
    let bytes = fs::read(run.workdir.join(file)).unwrap();
    let sha = sha256_hex(&bytes);

    assert!(
        run.shown_images()
            .contains(&("tool".to_string(), "image/png".to_string(), sha.clone())),
        "the driver was handed the image's bytes: {}",
        run.report
    );

    let record = run.record();
    let reference =
        json!({"type": "file", "sha256": sha, "media_type": "image/png", "bytes": bytes.len()});
    let line = record
        .lines()
        .find(|line| line.contains(&sha))
        .expect("a record line names the image");
    let message: Value = serde_json::from_str(line).unwrap();
    let image = message["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "image")
        .unwrap();
    assert_eq!(image["source"], reference);
    assert_eq!(
        message["content"][0]["type"], "text",
        "the text comes first"
    );
    assert!(
        !record.contains("\"data\""),
        "the record holds no image bytes"
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    assert!(!record.contains(&encoded));

    assert_eq!(fs::read(run.blob_dir().join(&sha)).unwrap(), bytes);
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(run.blob_dir()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "blobs/ is private");

    let call = run
        .tool_calls()
        .into_iter()
        .find(|call| call["attachments"].is_array())
        .expect("a tool_call names its attachments");
    assert_eq!(
        call["attachments"],
        json!([{"path": file, "media_type": "image/png", "bytes": bytes.len(), "sha256": sha}])
    );
    let trace = fs::read_to_string(run.records_dir.join("trace.jsonl")).unwrap();
    assert!(!trace.contains(&encoded), "the trace holds no image bytes");
}

#[test]
fn wasm_tool_png() {
    let run = run("img-wasm", false, "", "image-tool", json!([{"index": 1}])).unwrap();
    assert_attached(&run, "img-1.png");
}

#[test]
fn native_tool_png() {
    let Some(run) = run(
        "img-native",
        true,
        "",
        common::FIXTURE_NATIVE_TOOL_NAME,
        json!([{"operation": "image", "path": "shot.png", "index": 7}]),
    ) else {
        eprintln!("[SKIP] native_tool_png: the fixture native tool could not be built");
        return;
    };
    assert_attached(&run, "shot.png");
}

#[test]
fn text_named_png_is_refused() {
    let run = run(
        "img-text",
        false,
        "",
        "image-tool",
        json!([{"mode": "text_png"}]),
    )
    .unwrap();
    assert!(run.shown_images().is_empty(), "{}", run.report);
    assert!(!run.record().contains("\"type\":\"image\""));
    let record = run.record();
    let tool_line: Value = serde_json::from_str(
        record
            .lines()
            .find(|line| line.contains("\"role\":\"tool\""))
            .unwrap(),
    )
    .unwrap();
    let result = tool_line["content"][0]["text"].as_str().unwrap();
    let note_at = result.find("attachment 'note.png'").unwrap();
    let closing = result.rfind("attached note.png").unwrap();
    assert!(
        note_at > closing,
        "the note follows the fenced result: {result}"
    );
    assert!(
        result.trim_end().ends_with("(content_mismatch)"),
        "{result}"
    );
    let call = run.tool_calls()[0];
    assert_eq!(call["status"], "ok");
    assert_eq!(
        call["attachments"],
        json!([{"path": "note.png", "media_type": "image/png", "refused": "content_mismatch"}])
    );
    assert!(!run.blob_dir().exists(), "nothing was stored");
}

/// Each refusal, one tool call per turn, against `max_bytes: 1Ki`; then six images in one call.
#[test]
fn refusals() {
    let run = run(
        "img-refusals",
        false,
        "  images:\n    max_bytes: 1Ki\n",
        "image-tool",
        json!([
            {"mode": "svg"},
            {"mode": "big", "bytes": 2048},
            {"mode": "escape"},
            {"mode": "many", "count": 6}
        ]),
    )
    .unwrap();
    let calls = run.tool_calls();
    let refused = |index: usize| -> Vec<String> {
        calls[index]["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["refused"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(refused(0), ["media_type_not_allowed"]);
    assert_eq!(refused(1), ["too_large"]);
    assert_eq!(refused(2), ["outside_root"]);
    assert_eq!(refused(3), ["", "", "", "", "", "turn_limit"]);
    for call in &calls {
        assert_eq!(call["status"], "ok", "a refusal never changes the status");
    }
    let record = run.record();
    let big_note = record
        .lines()
        .find(|line| line.contains("attachment 'big.png'"))
        .unwrap();
    assert!(
        big_note.contains("2056 bytes") && big_note.contains("1024 bytes"),
        "{big_note}"
    );
    assert!(record.contains("attachment 'img-6.png' was not attached: "));
    assert_eq!(run.blob_names().len(), 5);
    assert_eq!(run.shown_images().len(), 5);

    // A native tool's entry that is not two strings is refused `malformed`.
    if let Some(native) = self::run(
        "img-malformed",
        true,
        "",
        common::FIXTURE_NATIVE_TOOL_NAME,
        json!([{"operation": "image", "malformed": true}]),
    ) {
        let call = native.tool_calls()[0];
        assert_eq!(call["attachments"][0]["refused"], "malformed", "{call}");
        assert!(native.record().contains("(malformed)"));
    } else {
        eprintln!("[SKIP] refusals: the fixture native tool could not be built");
    }
}

/// With six distinct images and the default `keep: 5`, the oldest reaches the driver as one line
/// naming it and the newest five as bytes; with five, all are bytes.
#[test]
fn images_age_out_after_keep() {
    let turns = |n: u64| Value::Array((1..=n).map(|index| json!({"index": index})).collect());

    let six = run("img-age-six", false, "", "image-tool", turns(6)).unwrap();
    let shas: Vec<String> = (1..=6)
        .map(|index| sha256_hex(&fs::read(six.workdir.join(format!("img-{index}.png"))).unwrap()))
        .collect();
    let shown: Vec<String> = six
        .shown_images()
        .into_iter()
        .map(|(_, _, sha)| sha)
        .collect();
    assert_eq!(shown, shas[1..].to_vec(), "{}", six.report);
    let aged = six
        .report
        .lines()
        .find(|line| line.contains("[image not shown: image/png"))
        .unwrap_or_else(|| panic!("the oldest image is named: {}", six.report));
    assert!(
        aged.contains(&format!("sha256 {}", &shas[0][..10])),
        "{aged}"
    );
    assert_eq!(six.blob_names().len(), 6);
    assert_eq!(six.record().matches("\"type\":\"file\"").count(), 6);

    let five = run("img-age-five", false, "", "image-tool", turns(5)).unwrap();
    assert_eq!(five.shown_images().len(), 5, "{}", five.report);
    assert!(!five.report.contains("[image not shown"));
    assert_eq!(five.blob_names().len(), 5);
    assert_eq!(five.record().matches("\"type\":\"file\"").count(), 5);
}

// ── Truncation ────────────────────────────────────────────────────────────────

fn image_line(index: u64, shas: &[&str]) -> String {
    let mut content = vec![json!({"type": "text", "text": format!("m{index}")})];
    for sha in shas {
        content.push(json!({"type": "image", "source":
            {"type": "file", "sha256": sha, "media_type": "image/png", "bytes": 4}}));
    }
    json!({"id": format!("msg_{:012x}{index:020x}", 1_700_000_000_000u64 + index),
        "role": "tool", "content": content})
    .to_string()
}

/// A record of four messages in `dir`: `old` named only by the first two, `kept` by the second
/// and the third, `new` by the fourth.
fn write_record_with_blobs(dir: &Path, capsule: &str) -> [String; 3] {
    let [old, kept, new] = ["a", "b", "c"].map(|c| c.repeat(64));
    fs::create_dir_all(dir.join("blobs")).unwrap();
    let header = json!({"type": "murmur.conversation", "capsule": capsule, "created_ms": 1_700_000_000_000u64});
    let lines = [
        header.to_string(),
        image_line(1, &[&old]),
        image_line(2, &[&old, &kept]),
        image_line(3, &[&kept]),
        image_line(4, &[&new]),
    ];
    fs::write(dir.join("conversation.jsonl"), lines.join("\n") + "\n").unwrap();
    for sha in [&old, &kept, &new] {
        fs::write(dir.join("blobs").join(sha), b"\x89PNG").unwrap();
    }
    [old, kept, new]
}

#[test]
fn conversation_truncate_removes_orphan_blobs() {
    // `mur conversation truncate`.
    let home = TempDir::new().unwrap();
    let dir = home
        .path()
        .join(".murmur/conversations/trunc-capsule/ctx-cli");
    let [old, kept, new] = write_record_with_blobs(&dir, "trunc-capsule");
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["conversation", "truncate", "ctx-cli", "--keep", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("dropped 2 messages"), "{stdout}");
    assert!(
        stdout.contains("removed 1 image blobs no kept message names"),
        "{stdout}"
    );
    assert!(!dir.join("blobs").join(&old).exists());
    assert!(dir.join("blobs").join(&kept).exists());
    assert!(dir.join("blobs").join(&new).exists());

    // `context.retain.max_messages` at launch removes the same set.
    let (home, artifacts) = publish_fixtures(false).unwrap();
    let project = TempDir::new().unwrap();
    let name = "trunc-launch";
    let dir = home
        .path()
        .join(".murmur/conversations")
        .join(name)
        .join("ctx-launch");
    let [old, kept, new] = write_record_with_blobs(&dir, name);
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest(name, false, "  retain:\n    max_messages: 2\n", ""),
    )
    .unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--context",
            "ctx-launch",
            "--task",
            "no probe",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.join("blobs").join(&old).exists());
    assert!(dir.join("blobs").join(&kept).exists());
    assert!(dir.join("blobs").join(&new).exists());
    drop(artifacts);
}

// ── A2A, the stream and the door ──────────────────────────────────────────────

const CREDENTIALS_YAML: &str = "network:\n  authentication:\n    scheme: bearer\n    \
     credentials:\n      alice:\n        scopes: [SendMessage, GetTask]\n";

/// A served capsule of the probe driver and the image tool, launched in process.
struct Door {
    addr: String,
    records_dir: PathBuf,
    workdir: PathBuf,
    tokens: Vec<(String, String)>,
    name: String,
    _dirs: (TempDir, TempDir, TempDir),
}

impl Door {
    fn start(name: &str, extra: &str) -> Door {
        let (home, artifacts) = publish_fixtures(false).unwrap();
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("murmur.yaml");
        fs::write(
            &manifest_path,
            manifest(
                name,
                false,
                "",
                &format!("lifecycle:\n  task_acceptance: queue\n  after_task: sleep\n  queue_depth: 8\n{extra}"),
            ),
        )
        .unwrap();
        let staged = stage_door_with_tools(&home, &manifest_path);
        let records_dir = staged.records_dir.clone();
        let workdir = staged.accessible_workdir.clone();
        let tokens = staged
            .door_tokens()
            .into_iter()
            .map(|(name, token)| (name, token.expose().to_string()))
            .collect();
        let addr = launch_door(staged);
        Door {
            addr,
            records_dir,
            workdir,
            tokens,
            name: name.to_string(),
            _dirs: (home, artifacts, project),
        }
    }

    fn token(&self, name: &str) -> String {
        self.tokens
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
            .unwrap()
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(&self.records_dir.join("trace.jsonl"))
    }

    /// Every conversation line this capsule recorded, across its contexts.
    fn records(&self) -> String {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let store = home.join(".murmur/conversations").join(&self.name);
        let mut all = String::new();
        for entry in fs::read_dir(store).into_iter().flatten().flatten() {
            all.push_str(
                &fs::read_to_string(entry.path().join("conversation.jsonl")).unwrap_or_default(),
            );
        }
        all
    }
}

fn png_part() -> Value {
    json!({"raw": base64::engine::general_purpose::STANDARD.encode(PNG_1X1), "mediaType": "image/png"})
}

fn send(door: &Door, token: Option<&str>, message_id: &str, parts: Value) -> Value {
    let response = rpc(
        &door.addr,
        token,
        "SendMessage",
        json!({"message": {"messageId": message_id, "role": "ROLE_USER", "parts": parts}}),
    );
    assert_eq!(response.status, 200, "{response:?}");
    response.json()
}

#[test]
fn a2a_image_part() {
    let door = Door::start("img-a2a", "");
    let sha = sha256_hex(&PNG_1X1);

    let answered = send(
        &door,
        None,
        "m-1",
        json!([{"text": "PROBE image-tool []", "mediaType": "text/plain"}, png_part()]),
    );
    let text = answered.to_string();
    let text_at = text
        .find("0 user text - - - ")
        .expect("the task's text block");
    let image_at = text
        .find(&format!("0 user image base64 image/png {sha}"))
        .unwrap_or_else(|| panic!("the image block: {text}"));
    assert!(text_at < image_at, "the image follows the text: {text}");
    let records = door.records();
    let reference = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| block["type"] == "image")
        .unwrap_or_else(|| panic!("the record holds the image: {records}"));
    assert_eq!(
        reference["source"],
        json!({"type": "file", "sha256": sha, "media_type": "image/png", "bytes": 67})
    );
    let start = door
        .trace()
        .into_iter()
        .find(|event| event["event_type"] == "task_start")
        .unwrap();
    assert_eq!(start["part_kinds"], json!(["text", "raw"]));
    assert_eq!(
        start["attachments"],
        json!([{"path": "message.parts[1]", "media_type": "image/png", "bytes": 67, "sha256": sha}])
    );

    // A `raw` part of another type keeps today's refusal.
    let other = send(
        &door,
        None,
        "m-2",
        json!([{"text": "hi"}, {"raw": "aGVsbG8=", "mediaType": "text/plain"}]),
    );
    assert_eq!(other["error"]["code"], -32005, "{other}");

    // A `raw` part declared image/png that holds text is refused as a whole, naming the part.
    let mismatched = send(
        &door,
        None,
        "m-3",
        json!([{"text": "hi"}, {"raw": "aGVsbG8=", "mediaType": "image/png"}]),
    );
    assert_eq!(mismatched["error"]["code"], -32602, "{mismatched}");
    let message = mismatched["error"]["message"].as_str().unwrap();
    assert!(message.contains("message.parts[1]"), "{message}");
    assert!(message.contains("context.images.max_bytes"), "{message}");

    // An image-only message starts a task.
    let alone = send(&door, None, "m-4", json!([png_part()]));
    let text = alone.to_string();
    assert!(
        text.contains(&format!("0 user image base64 image/png {sha}")),
        "{text}"
    );
}

/// Raw `GET`/`POST` of `path`, returning the status, the headers and the body's bytes.
fn http_bytes(addr: &str, method: &str, path: &str, token: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let auth = token
        .map(|token| format!("Authorization: {}\r\n", bearer(token)))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.to_lowercase(), raw[split + 4..].to_vec())
}

#[test]
fn stream_and_door_serve_attachments() {
    let door = Door::start("img-door", CREDENTIALS_YAML);
    let operator = door.token("operator");
    let (_, events) = common::stream::stream_message(
        &door.addr,
        Some(&operator),
        json!({"message": {"messageId": "m-1", "role": "ROLE_USER", "parts": [
            {"text": "PROBE image-tool [{\"index\": 1}, {\"mode\": \"none\"}]", "mediaType": "text/plain"}
        ]}}),
        true,
    );
    let frames: Vec<&Value> = events
        .iter()
        .filter(|event| event.kind() == "artifact")
        .map(|event| event.frame())
        .collect();
    assert_eq!(frames.len(), 2, "{events:?}");
    let task_id = events
        .iter()
        .find_map(|event| event.task_id().map(str::to_string))
        .unwrap();
    let png = fs::read(door.workdir.join("img-1.png")).unwrap();
    let sha = sha256_hex(&png);
    assert_eq!(
        frames[0]["attachments"],
        json!([{"mediaType": "image/png", "sizeBytes": png.len(), "sha256": sha,
            "url": format!("/attachments/{task_id}/{sha}")}])
    );
    assert_eq!(frames[1]["attachments"], json!([]));

    let url = format!("/attachments/{task_id}/{sha}");
    let (status, head, body) = http_bytes(&door.addr, "GET", &url, Some(&operator));
    assert_eq!(status, 200, "{head}");
    assert!(head.contains("content-type: image/png"), "{head}");
    assert!(
        head.contains(&format!("content-length: {}", png.len())),
        "{head}"
    );
    assert_eq!(body, png);

    assert_eq!(http_bytes(&door.addr, "GET", &url, None).0, 401);
    assert_eq!(
        http_bytes(&door.addr, "GET", &url, Some(&door.token("alice"))).0,
        404,
        "another credential cannot see the operator's task"
    );
    let unknown = format!("/attachments/{task_id}/{}", "0".repeat(64));
    assert_eq!(
        http_bytes(&door.addr, "GET", &unknown, Some(&operator)).0,
        404
    );
    assert_eq!(http_bytes(&door.addr, "POST", &url, Some(&operator)).0, 405);
    // The request helper's own reading of the door stays usable after these.
    let card = request(
        &door.addr,
        "GET",
        "/.well-known/agent-card.json",
        &[A2A_VERSION],
        "",
    );
    assert_eq!(card.status, 200);
}

// ── The scope report and the WIT level ────────────────────────────────────────

#[test]
fn explain_scope_reports_images() {
    let (home, _artifacts) = publish_fixtures(false).unwrap();
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("img-scope", false, "  images:\n    keep: 3\n", ""),
    )
    .unwrap();
    let human = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--explain-scope",
        ])
        .output()
        .unwrap();
    let human = String::from_utf8_lossy(&human.stdout).to_string();
    println!("{human}");
    assert!(
        human.contains(
            "\nImages\n  max_bytes:     5242880\n  keep:          3\n  max_per_turn:  5\n"
        ),
        "{human}"
    );
    let json = common::explain_scope_json(&home, &manifest_path);
    assert_eq!(
        json["images"],
        json!({"keep": 3, "max_bytes": 5_242_880, "max_per_turn": 5, "keep_applies": true})
    );

    // A process capsule: the harness holds the conversation.
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        "local/fixture-process-driver",
        VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    common::publish_local(&home, &driver).success();
    let process = project.path().join("process.yaml");
    fs::write(
        &process,
        format!(
            "name: img-process\nversion: 0.1.0\nartifacts:\n  - name: local/fixture-process-driver\n    \
             version: {VERSION}\n    runtime: driver\ncapabilities:\n  env:\n    allow: [HOME]\n\
             inference:\n  transport: process\n  driver:\n    artifact: local/fixture-process-driver\n"
        ),
    )
    .unwrap();
    let human = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "run",
            "--manifest",
            process.to_str().unwrap(),
            "--explain-scope",
        ])
        .output()
        .unwrap();
    let human = String::from_utf8_lossy(&human.stdout).to_string();
    println!("{human}");
    assert!(
        human.contains(
            "  keep:          5 (does nothing here: the harness holds the conversation)\n"
        ),
        "{human}"
    );
    assert!(human
        .contains("  max_per_turn:  5 (does nothing here: the harness holds the conversation)\n"));
    let json = common::explain_scope_json(&home, &process);
    assert_eq!(json["images"]["keep_applies"], false);
}

#[test]
fn old_level_tool_is_refused() {
    let (home, artifacts) = publish_fixtures(false).unwrap();
    let old = common::create_tool_artifact(
        artifacts.path(),
        "local/tool-v1",
        VERSION,
        &common::fixture_path("tool-v1/tool/jsonl-line-count-v1.wasm"),
    );
    common::publish_local(&home, &old).success();
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("img-old", false, "", "").replace(
            "capabilities:",
            &format!(
                "  - name: local/tool-v1\n    version: {VERSION}\n    runtime: tool\ncapabilities:"
            ),
        ),
    )
    .unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            "hi",
        ])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{text}");
    assert!(!output.status.success());
    assert!(text.contains("error[E-WIT-LEVEL-001]"), "{text}");
    assert!(
        text.contains(&format!(
            "artifact 'local/tool-v1@{VERSION}' does not fit WIT level 3, the level this mur \
             serves: it exports murmur:tool/run@0.1.0, and level 3 has murmur:tool@0.2.0"
        )),
        "{text}"
    );
}
