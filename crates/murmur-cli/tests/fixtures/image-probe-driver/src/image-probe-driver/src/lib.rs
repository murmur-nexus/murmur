// Test-only inference driver that is scripted by its task and reports, block by block, the request
// it was finally handed. It speaks the host's own driver request/response JSON and never makes an
// HTTP call.
//
// The task's first user message carries one line
//
//   PROBE <tool> <json array>
//
// and each element of the array is one model turn: an object is one call of <tool> with that
// object as its input, and an array of objects is that many calls in the same turn. The driver
// counts the tool results already in the request to know which turn it is on. Once every turn has
// been asked for it ends the task, and its final text, which the host writes to `out/result.txt`,
// is one line per content block of the request it received:
//
//   <message index> <role> <block type> <source.type> <media_type> <sha256> <text>
//
// where `<sha256>` is the sha256 of an image block's decoded base64 `data`, `<text>` is the first
// 60 characters of a text block with newlines shown as `\n`, and an absent field is `-`. A message
// whose content is a string reports one `string` line.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "driver",
    generate_all,
});

use base64::Engine as _;
use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The `PROBE` line's tool name and turns, read from the first user message's text.
fn script(messages: &[Value]) -> Option<(String, Vec<Value>)> {
    let first = messages
        .iter()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))?;
    let text = match first.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let line = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("PROBE "))?;
    let (tool, turns) = line.trim().split_once(' ')?;
    let turns: Vec<Value> = serde_json::from_str(turns.trim()).ok()?;
    Some((tool.to_string(), turns))
}

fn shown(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or("-")
        .to_string()
}

/// One line per content block of `messages`.
fn report(messages: &[Value]) -> String {
    let mut lines = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let role = shown(message.get("role"));
        let blocks = match message.get("content") {
            Some(Value::Array(blocks)) => blocks.clone(),
            Some(Value::String(text)) => {
                lines.push(format!("{index} {role} string - - - {}", preview(text)));
                continue;
            }
            _ => Vec::new(),
        };
        for block in blocks {
            let kind = shown(block.get("type"));
            let source = block.get("source");
            let source_type = shown(source.and_then(|s| s.get("type")));
            let media_type = shown(source.and_then(|s| s.get("media_type")));
            let sha = source
                .and_then(|s| s.get("data"))
                .and_then(Value::as_str)
                .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
                .map(|bytes| sha256_hex(&bytes))
                .unwrap_or_else(|| "-".to_string());
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .map(preview)
                .unwrap_or_else(|| "-".to_string());
            lines.push(format!(
                "{index} {role} {kind} {source_type} {media_type} {sha} {text}"
            ));
        }
    }
    lines.join("\n")
}

fn preview(text: &str) -> String {
    text.chars()
        .take(60)
        .collect::<String>()
        .replace('\n', "\\n")
}

fn respond(response: Value) -> ToolResult {
    ToolResult {
        status: Status::Passed,
        summary: Some("probed".to_string()),
        data: Some(response.to_string()),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

struct ImageProbeDriver;

impl Guest for ImageProbeDriver {
    fn run(input: ToolInput) -> ToolResult {
        let payload: Value =
            serde_json::from_str(&input.data.unwrap_or_default()).unwrap_or(Value::Null);
        let messages = payload
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let turns_taken = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .count();
        if let Some((tool, turns)) = script(&messages) {
            if let Some(turn) = turns.get(turns_taken) {
                let calls = match turn {
                    Value::Array(calls) => calls.clone(),
                    call => vec![call.clone()],
                };
                let content: Vec<Value> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| {
                        json!({
                            "type": "tool_call",
                            "id": format!("call_{turns_taken}_{index}"),
                            "name": tool,
                            "input": call,
                        })
                    })
                    .collect();
                return respond(json!({"stop_reason": "tool_call", "content": content}));
            }
        }
        respond(json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": report(&messages)}]
        }))
    }
}

export!(ImageProbeDriver);
