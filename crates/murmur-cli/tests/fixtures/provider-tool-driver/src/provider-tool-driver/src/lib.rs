// Test-only inference driver that is scripted by its task and reports what a provider-tool driver
// reads: each tool-array entry's `provider_tool` block, and each tool message's
// `acknowledged_safety_checks`. It speaks the host's own driver request/response JSON and never
// makes an HTTP call.
//
// The task's first user message carries one line
//
//   SCRIPT <json array>
//
// and each element of the array is one model turn: an object is one call, an array of objects is
// that many calls in the same turn. A call object is
//
//   {"name": <tool>, "input": <object>, "safety_checks": <any JSON>}
//
// where `input` defaults to `{}` and `safety_checks`, when present, is copied onto the call's
// `tool_call` block as it is. The driver counts the assistant messages already in the request to
// know which turn it is on. Once every turn has been asked for it ends the task, and its final
// text, which the host writes to `out/result.txt`, has one line per tool-array entry of the
// request it received, then one per tool message:
//
//   tool <name> <provider_tool>
//   message <tool_call_id> <is_error> <acknowledged_safety_checks> <text>
//
// `<provider_tool>` is the block as `{"kind":…,"display_width":…,"display_height":…}`, in that
// key order, and `<acknowledged_safety_checks>` the key's value as compact JSON; each is `-` when
// absent. `<text>` is the message's text blocks joined, with newlines shown as `\n`.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "driver",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use serde_json::{json, Value};

/// The `SCRIPT` line's turns, read from the first user message's text.
fn script(messages: &[Value]) -> Option<Vec<Value>> {
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
        .find_map(|line| line.trim().strip_prefix("SCRIPT "))?;
    serde_json::from_str(line.trim()).ok()
}

fn provider_tool(tool: &Value) -> String {
    let Some(block) = tool.get("provider_tool") else {
        return "-".to_string();
    };
    let field = |key: &str| block.get(key).cloned().unwrap_or(Value::Null).to_string();
    format!(
        "{{\"kind\":{},\"display_width\":{},\"display_height\":{}}}",
        field("kind"),
        field("display_width"),
        field("display_height")
    )
}

fn report(payload: &Value) -> String {
    let mut lines = Vec::new();
    for tool in payload
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("-");
        lines.push(format!("tool {name} {}", provider_tool(tool)));
    }
    for message in payload
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
    {
        let id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let is_error = message
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let acknowledged = message
            .get("acknowledged_safety_checks")
            .map_or_else(|| "-".to_string(), Value::to_string);
        let text = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        lines.push(format!(
            "message {id} {is_error} {acknowledged} {}",
            text.replace('\n', "\\n")
        ));
    }
    lines.join("\n")
}

fn respond(response: Value) -> ToolResult {
    ToolResult {
        status: Status::Passed,
        summary: Some("scripted".to_string()),
        data: Some(response.to_string()),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

struct ProviderToolDriver;

impl Guest for ProviderToolDriver {
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
        if let Some(turns) = script(&messages) {
            if let Some(turn) = turns.get(turns_taken) {
                let calls = match turn {
                    Value::Array(calls) => calls.clone(),
                    call => vec![call.clone()],
                };
                let content: Vec<Value> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| {
                        let mut block = json!({
                            "type": "tool_call",
                            "id": format!("call_{turns_taken}_{index}"),
                            "name": call.get("name").cloned().unwrap_or(Value::Null),
                            "input": call.get("input").cloned().unwrap_or_else(|| json!({})),
                        });
                        if let Some(checks) = call.get("safety_checks") {
                            block["safety_checks"] = checks.clone();
                        }
                        block
                    })
                    .collect();
                return respond(json!({"stop_reason": "tool_call", "content": content}));
            }
        }
        respond(json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": report(&payload)}]
        }))
    }
}

export!(ProviderToolDriver);
