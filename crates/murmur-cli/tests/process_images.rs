//! An image a tool attaches to its result, on `transport: process`: the tool bridge answers the
//! harness with the result's text and then an MCP `image` item per accepted attachment, and the
//! session's `tool_call` record carries `attachments` and never the image bytes.
//!
//! The counterpart to `images.rs`, which proves the same checks on `transport: http`. Everything
//! here runs a real fake harness against a real bridge; the harness's answer is the bridge's
//! JSON-RPC body, which it reports back as its tool result and as its final text.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::Engine as _;
use capsule_runtime::launch_session;
use serde_json::{json, Value};

const DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The native tool every scenario calls through the bridge.
const TOOL: &str = "png-tool";
/// The text the tool answers with, so its result's text item can be found.
const TOOL_OUTPUT: &str = "PNG-TOOL-RAN";

/// The PNG signature as `printf` octal escapes, then a short payload. The shell's `printf`
/// builtin writes it: a native tool execs only what `shell.allow` grants.
const PNG_PRINTF: &str = r"\211PNG\r\n\032\nIHDRpayload";

/// The bytes [`PNG_PRINTF`] writes.
fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(b"IHDRpayload");
    bytes
}

/// What one scenario's tool does and names.
struct Scenario<'a> {
    /// Shell lines the tool runs in the work folder before it answers.
    writes: String,
    /// The envelope's `attachments` value as JSON, or `None` for an envelope without the key.
    attachments: Option<String>,
    /// Extra manifest lines, top level.
    manifest_extra: &'a str,
}

impl Scenario<'_> {
    fn tool_script(&self) -> String {
        let attachments = self
            .attachments
            .as_ref()
            .map(|list| format!(r#","attachments":{list}"#))
            .unwrap_or_default();
        format!(
            "#!/bin/sh\n{}\necho '{{\"status\":\"passed\",\"summary\":\"ok\",\"data\":\"{TOOL_OUTPUT}\",\
             \"data_path\":null,\"truncated\":false,\"metadata\":[]{attachments}}}'\n",
            self.writes
        )
    }
}

/// What one launched session left behind.
struct Session {
    trace: Vec<Value>,
    trace_raw: String,
    workdir: PathBuf,
    records_dir: PathBuf,
    _project: tempfile::TempDir,
}

impl Session {
    /// The one `tool_call` record, with the whole trace in the failure message.
    fn tool_call(&self) -> &Value {
        let calls: Vec<&Value> = self
            .trace
            .iter()
            .filter(|e| e["event_type"] == "tool_call")
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "expected one tool_call record; trace was:\n{}",
            self.trace_raw
        );
        calls[0]
    }

    /// The `result` of the bridge's answer, which the harness reports as its final text.
    fn answer(&self) -> Value {
        let raw =
            fs::read_to_string(self.records_dir.join("out").join("result.txt")).unwrap_or_default();
        let body: Value = serde_json::from_str(raw.trim()).unwrap_or_else(|error| {
            panic!(
                "the harness's answer is the bridge's JSON body ({error}): {raw}\ntrace:\n{}",
                self.trace_raw
            )
        });
        body["result"].clone()
    }

    /// The answer's text item.
    fn answer_text(&self) -> String {
        let answer = self.answer();
        assert_eq!(answer["content"][0]["type"], "text", "{answer}");
        answer["content"][0]["text"].as_str().unwrap().to_string()
    }
}

fn write_harness(project: &Path, args: &str) -> PathBuf {
    let harness = project.join("harness.sh");
    let script = format!(
        "#!/bin/bash\nexport FIXTURE_HARNESS_PROFILE=bridge-call\nexport FIXTURE_BRIDGE_CALLS=1\n\
         export FIXTURE_BRIDGE_CALL_TOOL='{TOOL}'\nexport FIXTURE_BRIDGE_CALL_ARGS='{args}'\n\
         exec '{}' \"$@\"\n",
        common::fixture_path("process-driver/fake-harness").display()
    );
    fs::write(&harness, script).unwrap();
    let mut perms = fs::metadata(&harness).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&harness, perms).unwrap();
    harness
}

/// Publish the driver and the tool, stage the capsule, run the harness and collect what it left.
fn run_session(scenario: &Scenario<'_>) -> Session {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver = common::create_driver_artifact_with_auth(
        artifact_dir.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("process-driver/tool/process-driver.wasm"),
        "",
    );
    common::publish_local(&home, &driver).success();
    let tool = common::create_native_artifact(
        artifact_dir.path(),
        TOOL,
        "0.1.0",
        &scenario.tool_script(),
        Some("Fixture image tool"),
        None,
    );
    common::publish_local(&home, &tool).success();

    let harness = write_harness(project.path(), r#"{"shot":1}"#);
    let manifest = format!(
        "name: process-images\n\
         version: 0.1.0\n\
         artifacts:\n\
         \x20 - name: {DRIVER}\n\
         \x20   version: {DRIVER_VERSION}\n\
         \x20   runtime: driver\n\
         \x20 - name: local/{TOOL}\n\
         \x20   version: 0.1.0\n\
         \x20   runtime: tool\n\
         capabilities:\n\
         \x20 env:\n\
         \x20   allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
         trace:\n\
         \x20 capture: content\n\
         {extra}\
         inference:\n\
         \x20 transport: process\n\
         \x20 driver:\n\
         \x20   artifact: {DRIVER}\n\
         \x20 command: {harness}\n",
        extra = scenario.manifest_extra,
        harness = harness.display(),
    );
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.accessible_workdir.clone();
    let records_dir = staged.records_dir.clone();
    fs::write(workdir.join("task.md"), "Take the picture.").unwrap();

    launch_session(staged, |_| {}).expect("the session launches");

    let trace_raw = fs::read_to_string(records_dir.join("trace.jsonl")).unwrap_or_default();
    let trace = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line is JSON"))
        .collect();
    Session {
        trace,
        trace_raw,
        workdir,
        records_dir,
        _project: project,
    }
}

/// Every file beneath `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// The `attachments` entry of a refusal with `code`.
fn refused(path: &str, code: &str) -> Value {
    json!([{ "path": path, "media_type": "image/png", "refused": code }])
}

/// A tool that writes a PNG and names it: the answer is the text item, naming the image, then the
/// image item holding exactly the file's bytes; the record carries `attachments` and a
/// placeholder where the harness's report repeated the base64; nothing is stored.
#[test]
fn bridged_png_answers_text_then_image() {
    if common::skip_without_host_support("bridged_png_answers_text_then_image") {
        return;
    }
    let session = run_session(&Scenario {
        writes: format!("printf '{PNG_PRINTF}' > a.png"),
        attachments: Some(r#"[{"media_type":"image/png","path":"a.png"}]"#.to_string()),
        manifest_extra: "",
    });
    let png = fs::read(session.workdir.join("a.png")).unwrap();
    assert_eq!(png, png_bytes(), "the tool wrote the PNG");
    let sha = murmur_artifact::sha256_hex(&png);
    let n = png.len();

    let answer = session.answer();
    let content = answer["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "{answer}");
    let text = session.answer_text();
    assert!(text.contains(TOOL_OUTPUT), "{text}");
    assert!(
        text.contains(&format!("[image attached: a.png, image/png, {n} bytes]")),
        "{text}"
    );
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    assert_eq!(
        content[1],
        json!({ "type": "image", "data": b64, "mimeType": "image/png" })
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(content[1]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(murmur_artifact::sha256_hex(&decoded), sha);
    assert_eq!(answer["isError"], false);

    let call = session.tool_call();
    assert_eq!(
        call["attachments"],
        json!([{ "path": "a.png", "media_type": "image/png", "bytes": n, "sha256": sha }])
    );
    let output = call["output"]
        .as_str()
        .expect("content capture records output");
    assert!(
        output.contains(&format!("[image: image/png, {n} bytes, sha256 {sha}]")),
        "{output}"
    );
    assert!(!output.contains(&b64), "{output}");
    assert_eq!(call["output_bytes"], output.len());

    let conversations = common::process_home().join(".murmur").join("conversations");
    for path in files_under(&session.records_dir)
        .into_iter()
        .chain(files_under(&conversations))
    {
        // `trace.capture: content` keeps the wire text in `blobs/`, the harness's echo of the
        // answer included; no file is the image itself, or named by its sha256.
        assert_ne!(
            path.file_name().and_then(|name| name.to_str()),
            Some(sha.as_str()),
            "the image was stored as a blob: {}",
            path.display()
        );
        assert_ne!(
            fs::read(&path).unwrap_or_default(),
            png,
            "the image was stored at {}",
            path.display()
        );
    }
    assert!(
        files_under(&conversations).is_empty(),
        "nothing is written under ~/.murmur/conversations/: {:?}",
        files_under(&conversations)
    );
}

/// A tool that attaches nothing gets the answer the bridge has always given: one text item and
/// `isError`, nothing else, and its record has no `attachments` key.
#[test]
fn bridged_call_without_attachment_answers_as_today() {
    if common::skip_without_host_support("bridged_call_without_attachment_answers_as_today") {
        return;
    }
    let session = run_session(&Scenario {
        writes: format!("printf '{PNG_PRINTF}' > a.png"),
        attachments: None,
        manifest_extra: "",
    });
    let answer = session.answer();
    let mut keys: Vec<&String> = answer.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["content", "isError"], "{answer}");
    let content = answer["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "{answer}");
    let mut item_keys: Vec<&String> = content[0].as_object().unwrap().keys().collect();
    item_keys.sort();
    assert_eq!(item_keys, ["text", "type"]);
    let text = session.answer_text();
    assert!(text.contains(TOOL_OUTPUT), "{text}");
    assert!(!text.contains("[image attached"), "{text}");
    let call = session.tool_call();
    assert!(call.get("attachments").is_none(), "{call}");
}

/// A text file declared as a PNG, a PNG over `context.images.max_bytes` and a path outside the
/// work folder are each refused with their own code: the answer carries the refusal line and no
/// image item, the record the one refused entry, and the call still succeeds.
#[test]
fn bridged_refusals() {
    if common::skip_without_host_support("bridged_refusals") {
        return;
    }
    // 8 signature bytes, `IHDRpayload`, then 40 × 40 bytes: 1619 bytes.
    let big = format!(
        "printf '{PNG_PRINTF}' > big.png\ni=0\nwhile [ $i -lt 40 ]; do \
         printf '0123456789012345678901234567890123456789' >> big.png; i=$((i+1)); done"
    );
    let cases = [
        (
            "printf 'just some text' > note.png".to_string(),
            "note.png",
            "",
            "content_mismatch",
        ),
        (
            big,
            "big.png",
            "context:\n  images:\n    max_bytes: 1Ki\n",
            "too_large",
        ),
        (String::new(), "../escape.png", "", "outside_root"),
    ];
    for (writes, path, manifest_extra, code) in cases {
        let session = run_session(&Scenario {
            writes,
            attachments: Some(format!(r#"[{{"media_type":"image/png","path":"{path}"}}]"#)),
            manifest_extra,
        });
        let answer = session.answer();
        assert_eq!(
            answer["content"].as_array().unwrap().len(),
            1,
            "{code}: {answer}"
        );
        assert_eq!(answer["isError"], false, "{code}: {answer}");
        let text = session.answer_text();
        let line = text.lines().last().unwrap();
        assert!(
            line.starts_with(&format!("attachment '{path}' was not attached: "))
                && line.ends_with(&format!("({code})")),
            "{code}: {line}"
        );
        if code == "too_large" {
            assert!(
                line.contains("1619 bytes is over context.images.max_bytes of 1024 bytes"),
                "{line}"
            );
        }
        assert!(!text.contains("[image attached"), "{text}");
        let call = session.tool_call();
        assert_eq!(call["attachments"], refused(path, code), "{call}");
        assert_eq!(call["status"], "ok", "{call}");
    }
}

/// Six valid PNGs named by one call, under the default `context.images`: the first five are image
/// items, the sixth is refused with `turn_limit`.
#[test]
fn bridged_call_caps_images_per_call() {
    if common::skip_without_host_support("bridged_call_caps_images_per_call") {
        return;
    }
    let writes: Vec<String> = (0..6)
        .map(|i| format!("printf '{PNG_PRINTF}{i}' > {i}.png"))
        .collect();
    let attachments: Vec<String> = (0..6)
        .map(|i| format!(r#"{{"media_type":"image/png","path":"{i}.png"}}"#))
        .collect();
    let session = run_session(&Scenario {
        writes: writes.join("\n"),
        attachments: Some(format!("[{}]", attachments.join(","))),
        manifest_extra: "",
    });
    let answer = session.answer();
    let content = answer["content"].as_array().unwrap();
    assert_eq!(content.len(), 6, "one text item and five images: {answer}");
    for (i, item) in content[1..].iter().enumerate() {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(item["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            decoded,
            fs::read(session.workdir.join(format!("{i}.png"))).unwrap()
        );
    }
    let text = session.answer_text();
    for i in 0..5 {
        assert!(
            text.contains(&format!("[image attached: {i}.png, image/png, ")),
            "{text}"
        );
    }
    let line = text.lines().last().unwrap();
    assert!(
        line.starts_with("attachment '5.png' was not attached: ") && line.ends_with("(turn_limit)"),
        "{line}"
    );
    let entries = session.tool_call()["attachments"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(entries.len(), 6);
    assert!(entries[..5].iter().all(|e| e["sha256"].is_string()));
    assert_eq!(entries[5], refused("5.png", "turn_limit")[0]);
}
