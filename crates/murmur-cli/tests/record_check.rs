//! The record check over real launches: a rewritten conversation that breaks a structural rule is
//! refused whole, the trace names every fault, and the record the session had is what every later
//! launch continues from.
//!
//! Every case drives the real `mur` binary under a scratch `HOME`, a real Wasmtime driver and a
//! scripted provider. The compaction hooks are hand-authored WAT returning exactly the messages
//! each case names, so a refused replacement is a replacement the test wrote byte for byte.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, Command};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::hook_wat::{compaction_hook_wasm_returning, create_hook_zip};

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const OPENAI: &str = "local/murmur-driver-openai";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE: &str = "record-check-capsule";
const COMPACTOR: &str = "local/compactor";
const TASK: &str = "Run echo and summarize.";

/// `context.max_tokens`, `inference.max_tokens` and the threshold: the input budget is 3000 and
/// compaction fires at 1500 tokens, which only a turn reporting [`TRIPPING_INPUT`] reaches.
const COMPACTING: &str = "context:\n  max_tokens: 4000\n";
const COMPACTION_BLOCK: &str = "  max_tokens: 1000\n  compaction:\n    threshold: 0.5\n";
const TRIPPING_INPUT: u64 = 2000;
const QUIET_INPUT: u64 = 10;

/// The hook's call: the very call the scripted first turn makes.
const HOOK_CALL: &str =
    r#"[{"type":"tool_call","id":"toolu_ls","name":"bash","input":{"command":"echo hello"}}]"#;
/// The runtime's tool-result wrapper for `toolu_ls`, cut off inside its body.
const CUT_RESULT: &str = r#"{"__murmur_tool_msg__":true,"tool_call_id":"toolu_ls","is_error":false,"body":[{"type":"text","text":"hel"#;
/// A call whose arguments are a string that does not parse.
const STRING_ARGS_CALL: &str =
    r#"[{"type":"tool_call","id":"toolu_ls","name":"bash","input":"{\"command\": \"echo hel"}]"#;
/// An intact wrapper answering `toolu_ls`.
const INTACT_RESULT: &str = r#"{"__murmur_tool_msg__":true,"tool_call_id":"toolu_ls","is_error":false,"body":[{"type":"text","text":"hello"}]}"#;
/// The text a refusing hook's messages carry in the resume case, so any request or record line
/// that held one of them is found by a substring.
const SENTINEL: &str = "HOOK-SENTINEL";

// ── Scripted provider replies ────────────────────────────────────────────────

/// An Anthropic turn calling `bash` with `command` under the id `toolu_ls`.
fn tool_turn(command: &str, input_tokens: u64) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": "toolu_ls",
            "name": "bash",
            "input": {"command": command}
        }],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input_tokens, "output_tokens": 50}
    })
    .to_string()
}

/// An Anthropic turn that ends the task saying `text`.
fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": QUIET_INPUT, "output_tokens": 5}
    })
    .to_string()
}

/// An OpenAI chat completion saying `text`: `test-model` is served on that surface.
fn chat_text(text: &str) -> String {
    json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": text}
        }]
    })
    .to_string()
}

// ── Homes, manifests and runs ────────────────────────────────────────────────

/// A scratch `HOME` with both fixture drivers published, and the compactor built from `hook`
/// when one is given.
struct Home {
    dir: TempDir,
    _artifacts: TempDir,
}

impl Home {
    fn new(hook: Option<&[(&str, &str)]>) -> Self {
        let dir = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        for (name, wasm) in [
            (
                ANTHROPIC,
                "drivers/anthropic/driver/murmur-driver-anthropic.wasm",
            ),
            (OPENAI, "drivers/openai/driver/murmur-driver-openai.wasm"),
        ] {
            let driver = common::create_driver_artifact(
                artifacts.path(),
                name,
                DRIVER_VERSION,
                &common::fixture_path(wasm),
            );
            common::publish_local(&dir, &driver).success();
        }
        if let Some(messages) = hook {
            let artifact = create_hook_zip(
                artifacts.path(),
                COMPACTOR,
                "on-compaction",
                "replace-context",
                &compaction_hook_wasm_returning(messages),
            );
            common::publish_local(&dir, &artifact).success();
        }
        Self {
            dir,
            _artifacts: artifacts,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn record_path(&self, context_id: &str) -> PathBuf {
        self.path()
            .join(".murmur/conversations")
            .join(CAPSULE)
            .join(context_id)
            .join("conversation.jsonl")
    }

    /// Every non-empty line of one context's record.
    fn record_lines(&self, context_id: &str) -> Vec<String> {
        let path = self.record_path(context_id);
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// What a capsule manifest declares beside its name.
struct Capsule<'a> {
    driver: &'a str,
    endpoint: &'a str,
    /// Bind the compactor and set the compaction threshold.
    compacting: bool,
    /// Extra top-level YAML.
    blocks: &'a str,
}

/// A capsule on `driver` reaching `endpoint`, with no extra blocks.
fn capsule<'a>(driver: &'a str, endpoint: &'a str, compacting: bool) -> Capsule<'a> {
    Capsule {
        driver,
        endpoint,
        compacting,
        blocks: "",
    }
}

fn write_manifest(project: &Path, capsule: &Capsule<'_>) -> PathBuf {
    let Capsule {
        driver,
        endpoint,
        compacting,
        blocks,
    } = capsule;
    let (context, hook, compaction) = if *compacting {
        (
            COMPACTING,
            format!("  - name: {COMPACTOR}\n    version: 0.1.0\n    runtime: hook\n"),
            COMPACTION_BLOCK,
        )
    } else {
        ("", String::new(), "")
    };
    let manifest = format!(
        "name: {CAPSULE}\nversion: 0.1.0\n{context}{blocks}artifacts:\n  - name: {driver}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      \
         endpoint: {endpoint}\n      api_key: test-key\n{hook}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\ninference:\n  \
         transport: http\n  model: test-model\n{compaction}  driver:\n    artifact: {driver}\n",
    );
    let path = project.join("murmur.yaml");
    fs::write(&path, manifest).unwrap();
    path
}

/// `mur run --verbose` over `manifest` with `task`, plus `extra`.
fn run(home: &Home, manifest: &Path, task: &str, extra: &[&str]) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path());
    cmd.args([
        "run",
        "--manifest",
        manifest.to_str().unwrap(),
        "--task",
        task,
        "--verbose",
    ]);
    cmd.args(extra);
    cmd.assert()
}

/// The records directory a successful run reported.
fn records_of(assert: Assert) -> PathBuf {
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    common::parse_records_from_stdout(&stdout)
}

/// Every line of a session's trace. A `record_check_failed` line is also printed as written, so
/// a run under `--no-capture` shows it with its keys in file order.
fn trace_events(records: &Path) -> Vec<Value> {
    fs::read_to_string(records.join("trace.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .inspect(|line| {
            if line.starts_with(r#"{"event_type":"record_check_failed""#) {
                eprintln!("record_check_failed line: {line}");
            }
        })
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn events_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["event_type"] == kind).collect()
}

/// The one `record_check_failed` line of a trace.
fn the_record_check_failed(events: &[Value]) -> Value {
    let failed = events_of(events, "record_check_failed");
    assert_eq!(failed.len(), 1, "{events:#?}");
    failed[0].clone()
}

/// `(check, index, tool_call_id)` of every listed fault.
fn faults(line: &Value) -> Vec<(String, u64, Value)> {
    line["faults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fault| {
            (
                fault["check"].as_str().unwrap().to_string(),
                fault["index"].as_u64().unwrap(),
                fault["tool_call_id"].clone(),
            )
        })
        .collect()
}

/// A refused compaction: one `compaction_declined` with the record check as its reason, and no
/// `compaction`.
fn assert_declined_by_the_check(events: &[Value]) {
    let declined = events_of(events, "compaction_declined");
    assert_eq!(declined.len(), 1, "{declined:?}");
    assert_eq!(declined[0]["reason"], "record_check_failed");
    assert!(events_of(events, "compaction").is_empty());
}

/// The context id a launch's task ran under.
fn context_of(events: &[Value]) -> String {
    events_of(events, "task_start")[0]["context_id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The record's message lines, without its header.
fn message_lines(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|line| serde_json::from_str::<Value>(line).unwrap()["type"] != "murmur.record")
        .collect()
}

/// The text of the record's one tool result, JSON-escaped as it appears inside any request body
/// that carries it.
fn recorded_result_needle(lines: &[String]) -> String {
    let tool = lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|message| message["role"] == "tool")
        .expect("the first launch recorded the tool result");
    let text: String = tool["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert!(text.contains("hello"), "{tool}");
    let quoted = serde_json::to_string(&text).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

/// The position of the first request message `matches` accepts.
fn position(messages: &[Value], what: &str, matches: impl Fn(&Value) -> bool) -> usize {
    messages
        .iter()
        .position(matches)
        .unwrap_or_else(|| panic!("no message is {what}: {messages:#?}"))
}

fn text_contains(message: &Value, text: &str) -> bool {
    message.to_string().contains(text)
}

// ── Compaction ───────────────────────────────────────────────────────────────

/// A hook that returns the first turn's call with a tool result cut off inside its JSON: the
/// result cannot be read, so it is an orphan at its own position, and the call it was meant to
/// answer is unanswered. Refused; the session runs on and its next request carries the call and
/// the result the turn really produced.
#[test]
fn a_truncated_tool_result_is_refused_and_the_launch_continues() {
    if common::skip_without_host_support(
        "a_truncated_tool_result_is_refused_and_the_launch_continues",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_turn("echo hello", TRIPPING_INPUT),
        end_turn("Task complete."),
    ]);
    let home = Home::new(Some(&[("assistant", HOOK_CALL), ("tool", CUT_RESULT)]));
    let project = TempDir::new().unwrap();
    let manifest = write_manifest(
        project.path(),
        &Capsule {
            driver: ANTHROPIC,
            endpoint: &server.endpoint,
            compacting: true,
            blocks: "",
        },
    );

    let assert = run(&home, &manifest, TASK, &[]).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("ok"), "{stdout}");
    let records = records_of(assert);
    let events = trace_events(&records);

    let failed = the_record_check_failed(&events);
    assert_eq!(failed["rewrite"], "compaction");
    assert_eq!(failed["messages"], 2);
    assert_eq!(failed["faults_total"], 2);
    assert_eq!(
        faults(&failed),
        [
            ("unanswered_tool_call".to_string(), 0, json!("toolu_ls")),
            ("orphaned_tool_result".to_string(), 1, Value::Null),
        ]
    );
    assert_declined_by_the_check(&events);

    let show = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "trace",
            "show",
            records.join("trace.jsonl").to_str().unwrap(),
        ])
        .assert()
        .success();
    let show = String::from_utf8(show.get_output().stdout.clone()).unwrap();
    assert!(show.contains("── Record check"), "{show}");
    assert!(
        show.contains(
            "record_check_failed  compaction  2 faults  first: unanswered_tool_call at 0"
        ),
        "{show}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1]["messages"].as_array().unwrap();
    let call = position(second, "the toolu_ls call", |m| {
        m["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b["type"] == "tool_use"
                    && b["id"] == "toolu_ls"
                    && b["input"] == json!({"command": "echo hello"})
            })
        })
    });
    let result = position(second, "the toolu_ls result", |m| {
        m["content"].as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_ls")
        })
    });
    assert!(call < result);
    assert_eq!(
        requests[1].to_string().matches("\"tool_use\"").count(),
        1,
        "one call, the model's own: {}",
        requests[1]
    );

    let lines = home.record_lines(&context_of(&events));
    let messages = message_lines(&lines);
    assert_eq!(messages.len(), 4, "task, call, result, answer: {lines:#?}");
    assert!(lines
        .iter()
        .all(|line| !line.contains("__murmur_tool_msg__")));
}

/// A hook whose call carries its arguments as a string that does not parse: exactly one fault,
/// and neither the trace nor `bootstrap.log` repeats the arguments. The model's own call runs a
/// different command, so the hook's text could only be there if it leaked.
#[test]
fn malformed_arguments_are_refused_without_echoing_them() {
    if common::skip_without_host_support("malformed_arguments_are_refused_without_echoing_them") {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_turn("echo hi", TRIPPING_INPUT),
        end_turn("Task complete."),
    ]);
    let home = Home::new(Some(&[
        ("assistant", STRING_ARGS_CALL),
        ("tool", INTACT_RESULT),
    ]));
    let project = TempDir::new().unwrap();
    let manifest = write_manifest(
        project.path(),
        &Capsule {
            driver: ANTHROPIC,
            endpoint: &server.endpoint,
            compacting: true,
            blocks: "",
        },
    );

    let records = records_of(run(&home, &manifest, TASK, &[]).success());
    let events = trace_events(&records);
    let failed = the_record_check_failed(&events);
    assert_eq!(failed["faults_total"], 1);
    assert_eq!(
        faults(&failed),
        [(
            "unparseable_tool_arguments".to_string(),
            0,
            json!("toolu_ls")
        )]
    );
    assert_declined_by_the_check(&events);

    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let bootstrap = fs::read_to_string(records.join("logs/bootstrap.log")).unwrap();
    assert!(
        bootstrap.contains(
            "[compaction] the replacement failed the record check (1 faults; first: \
             unparseable_tool_arguments at 0); continuing without compaction"
        ),
        "{bootstrap}"
    );
    for text in [&trace, &bootstrap] {
        assert!(!text.contains("echo hel"), "{text}");
    }

    let lines = home.record_lines(&context_of(&events));
    assert_eq!(message_lines(&lines).len(), 4, "{lines:#?}");
    assert!(lines
        .iter()
        .all(|line| !line.contains("echo hel") && !line.contains("__murmur_tool_msg__")));
}

/// A refused compaction, then `--resume @1` under the Anthropic driver, then the same capsule
/// moved to the OpenAI driver and resumed again. Each resumed launch's first request carries the
/// original task, the original call and the result it really got, in that driver's own wire
/// shape, then its new task — and nothing the refusing hook returned. The resumed launches bind
/// no compaction hook.
#[test]
fn resume_after_refusal_continues_from_the_original_messages_under_both_drivers() {
    if common::skip_without_host_support(
        "resume_after_refusal_continues_from_the_original_messages_under_both_drivers",
    ) {
        return;
    }
    let anthropic = common::ScriptedServer::start(vec![
        tool_turn("echo hello", TRIPPING_INPUT),
        end_turn("Task complete."),
        end_turn("anthropic resumed"),
    ]);
    let openai = common::ScriptedServer::start(vec![chat_text("openai resumed")]);
    let hook_call = format!(
        r#"[{{"type":"text","text":"{SENTINEL}-PLAN"}},{{"type":"tool_call","id":"toolu_ls","name":"bash","input":{{"command":"echo hello"}}}}]"#
    );
    let hook_result = format!(
        r#"{{"__murmur_tool_msg__":true,"tool_call_id":"toolu_ls","is_error":false,"body":[{{"type":"text","text":"{SENTINEL}-RESULT"#
    );
    let home = Home::new(Some(&[("assistant", &hook_call), ("tool", &hook_result)]));
    let project = TempDir::new().unwrap();

    let manifest = write_manifest(
        project.path(),
        &capsule(ANTHROPIC, &anthropic.endpoint, true),
    );
    let records = records_of(run(&home, &manifest, "first task: run echo", &[]).success());
    let events = trace_events(&records);
    let failed = the_record_check_failed(&events);
    assert_eq!(
        faults(&failed),
        [
            ("unanswered_tool_call".to_string(), 0, json!("toolu_ls")),
            ("orphaned_tool_result".to_string(), 1, Value::Null),
        ]
    );
    assert_declined_by_the_check(&events);
    let context = context_of(&events);
    let needle = recorded_result_needle(&home.record_lines(&context));

    write_manifest(
        project.path(),
        &capsule(ANTHROPIC, &anthropic.endpoint, false),
    );
    run(&home, &manifest, "second task", &["--resume", "@1"]).success();
    let request = anthropic.requests()[2].clone();
    let messages = request["messages"].as_array().unwrap();
    let task = position(messages, "the original task", |m| {
        m["role"] == "user" && text_contains(m, "first task: run echo")
    });
    let call = position(messages, "the original call", |m| {
        m["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b["type"] == "tool_use"
                    && b["id"] == "toolu_ls"
                    && b["input"] == json!({"command": "echo hello"})
            })
        })
    });
    let result = position(messages, "the recorded result", |m| {
        m["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b["type"] == "tool_result"
                    && b["tool_use_id"] == "toolu_ls"
                    && text_contains(b, &needle)
            })
        })
    });
    let new_task = position(messages, "the new task", |m| {
        m["role"] == "user" && text_contains(m, "second task")
    });
    assert!(
        task < call && call < result && result < new_task,
        "{request}"
    );
    assert!(!request.to_string().contains(SENTINEL), "{request}");

    // The lock pins the Anthropic driver; the switched capsule resolves its own.
    fs::remove_file(project.path().join("murmur.lock")).unwrap();
    write_manifest(project.path(), &capsule(OPENAI, &openai.endpoint, false));
    run(&home, &manifest, "third task", &["--resume", "@1"]).success();
    let request = openai.requests()[0].clone();
    let messages = request["messages"].as_array().unwrap();
    let task = position(messages, "the original task", |m| {
        m["role"] == "user" && text_contains(m, "first task: run echo")
    });
    let call = position(messages, "the original call", |m| {
        m["tool_calls"].as_array().is_some_and(|calls| {
            calls.iter().any(|c| {
                c["id"] == "toolu_ls"
                    && c["function"]["name"] == "bash"
                    && c["function"]["arguments"]
                        .as_str()
                        .and_then(|args| serde_json::from_str::<Value>(args).ok())
                        == Some(json!({"command": "echo hello"}))
            })
        })
    });
    let result = position(messages, "the recorded result", |m| {
        m["role"] == "tool" && m["tool_call_id"] == "toolu_ls" && text_contains(m, &needle)
    });
    let new_task = position(messages, "the new task", |m| {
        m["role"] == "user" && text_contains(m, "third task")
    });
    assert!(
        task < call && call < result && result < new_task,
        "{request}"
    );
    assert!(!request.to_string().contains(SENTINEL), "{request}");

    let lines = home.record_lines(&context);
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(SENTINEL) && !line.contains("__murmur_tool_msg__")),
        "{lines:#?}"
    );
}

/// `--resume-mode compact` goes through the same check: the hook's replacement is refused, the
/// launch runs on the verbatim loaded history, and the lines it loaded are unchanged.
#[test]
fn resume_mode_compact_refused_keeps_the_loaded_history() {
    if common::skip_without_host_support("resume_mode_compact_refused_keeps_the_loaded_history") {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_turn("echo hello", QUIET_INPUT),
        end_turn("Task complete."),
        end_turn("resumed"),
    ]);
    let home = Home::new(Some(&[
        ("assistant", STRING_ARGS_CALL),
        ("tool", INTACT_RESULT),
    ]));
    let project = TempDir::new().unwrap();
    let manifest = write_manifest(
        project.path(),
        &Capsule {
            driver: ANTHROPIC,
            endpoint: &server.endpoint,
            compacting: true,
            blocks: "",
        },
    );

    let first = trace_events(&records_of(
        run(&home, &manifest, TASK, &["--context", "ctx_rc"]).success(),
    ));
    assert!(events_of(&first, "compaction_declined").is_empty());
    let before = home.record_lines("ctx_rc");
    assert_eq!(message_lines(&before).len(), 4, "{before:#?}");

    let records = records_of(
        run(
            &home,
            &manifest,
            "second task",
            &["--resume", "@1", "--resume-mode", "compact"],
        )
        .success(),
    );
    let events = trace_events(&records);
    let failed = the_record_check_failed(&events);
    assert_eq!(failed["rewrite"], "compaction");
    assert_eq!(
        faults(&failed),
        [(
            "unparseable_tool_arguments".to_string(),
            0,
            json!("toolu_ls")
        )]
    );
    assert_declined_by_the_check(&events);

    let request = server.requests()[2].clone();
    let messages = request["messages"].as_array().unwrap();
    let task = position(messages, "the original task", |m| text_contains(m, TASK));
    let call = position(messages, "the original call", |m| {
        text_contains(m, "\"tool_use\"") && text_contains(m, "toolu_ls")
    });
    let result = position(messages, "the original result", |m| {
        text_contains(m, "\"tool_result\"") && text_contains(m, "toolu_ls")
    });
    let answer = position(messages, "the original answer", |m| {
        text_contains(m, "Task complete.")
    });
    let new_task = position(messages, "the new task", |m| {
        text_contains(m, "second task")
    });
    assert!(
        task < call && call < result && result < answer && answer < new_task,
        "{request}"
    );

    let after = home.record_lines("ctx_rc");
    assert_eq!(&after[..before.len()], &before[..]);
    assert!(after
        .iter()
        .all(|line| !line.contains("__murmur_tool_msg__")));
}

// ── Truncation ───────────────────────────────────────────────────────────────

/// `[user, assistant(tool_call c1), tool(c1), assistant text]` as record lines.
fn tool_turn_record() -> Vec<String> {
    [
        json!({"id": "msg_0001", "role": "user", "content": [{"type": "text", "text": "go"}]}),
        json!({"id": "msg_0002", "role": "assistant", "content": [{"type": "tool_call", "id": "c1", "name": "bash", "input": {"command": "ls"}}]}),
        json!({"id": "msg_0003", "role": "tool", "tool_call_id": "c1", "is_error": false, "content": [{"type": "text", "text": "ok"}]}),
        json!({"id": "msg_0004", "role": "assistant", "content": [{"type": "text", "text": "done"}]}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

fn plant_record(home: &Home, context_id: &str) -> PathBuf {
    let path = home.record_path(context_id);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, tool_turn_record().join("\n") + "\n").unwrap();
    path
}

/// `context.retain.max_messages: 2` over one tool turn would keep a tool result whose call it
/// dropped: the launch writes `record_check_failed` and no `retention` line, runs to its end,
/// and the record keeps every line it had, ahead of the launch's own.
#[test]
fn a_max_messages_truncation_that_orphans_a_tool_result_is_refused_at_launch() {
    let server = common::ScriptedServer::start(vec![end_turn("reply")]);
    let home = Home::new(None);
    let path = plant_record(&home, "ctx_trunc");
    let project = TempDir::new().unwrap();
    let manifest = write_manifest(
        project.path(),
        &Capsule {
            driver: ANTHROPIC,
            endpoint: &server.endpoint,
            compacting: false,
            blocks: "context:\n  record: on\n  retain:\n    max_messages: 2\n",
        },
    );

    let assert = run(&home, &manifest, TASK, &["--context", "ctx_trunc"]).success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let events = trace_events(&records_of(assert));
    let failed = the_record_check_failed(&events);
    assert_eq!(failed["rewrite"], "truncation");
    assert_eq!(failed["turn"], Value::Null);
    assert_eq!(failed["messages"], 2);
    assert_eq!(
        faults(&failed),
        [("orphaned_tool_result".to_string(), 0, json!("c1"))]
    );
    assert_eq!(failed["faults"][0]["message_id"], "msg_0003");
    assert!(events_of(&events, "retention")
        .iter()
        .all(|e| e["reason"] != "max_messages"));
    assert!(
        stderr.contains("was not truncated: the kept messages fail the record check (1 faults; first: orphaned_tool_result at 0); this record keeps growing"),
        "{stderr}"
    );

    let lines: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let messages = message_lines(&lines);
    let original = tool_turn_record();
    assert!(messages.len() > original.len(), "{lines:#?}");
    assert_eq!(
        messages[..original.len()]
            .iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>(),
        original.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(lines
        .iter()
        .all(|line| serde_json::from_str::<Value>(line).unwrap()["truncated"].is_null()));
}

/// `mur conversation truncate --keep 2` over the same record is refused with `E-CNV-004` naming
/// the rule, the position and the tool call, and changes nothing; `--keep 1` truncates.
#[test]
fn conversation_truncate_refuses_a_keep_that_orphans_a_tool_result() {
    let home = Home::new(None);
    let path = plant_record(&home, "ctx_cli");
    let before = fs::read(&path).unwrap();
    let truncate = |keep: &str| {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .args(["conversation", "truncate", "ctx_cli", "--keep", keep])
            .assert()
    };

    let refused = truncate("2").failure();
    let stderr = String::from_utf8(refused.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("error[E-CNV-004]"), "{stderr}");
    assert!(
        stderr.contains(
            "keeping the newest 2 messages would leave orphaned_tool_result at index 0 (tool call c1)"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fs::read(&path).unwrap(), before);

    truncate("1").success();
    let kept: Vec<Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|line: &Value| line["type"] != "murmur.record")
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0]["id"], "msg_0004");
}
