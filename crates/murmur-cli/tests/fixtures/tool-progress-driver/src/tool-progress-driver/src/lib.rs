// Test-only inference driver that reports a tool call's start and the size of its input through
// `murmur:stream/events` before returning the call. It speaks the host's own driver JSON and never
// makes an HTTP call.
//
// The mode is `MURMUR_INFERENCE_MODEL`, the manifest's `inference.model`. Turn 2 — any request
// carrying a `tool` message — answers `PROGRESS-ANSWER` in every mode, without streaming. Turn 1,
// by mode:
//
//   * `tool-progress` — starts `c1` as `echo-tool`, reports half of `INPUT`'s size and then all of
//     it, and returns the call.
//   * `tool-progress-streaming` — a thinking chunk and a text chunk first, then the same as
//     `tool-progress`; the returned content is the text block followed by the call.
//   * `tool-progress-large` — starts `c1` and reports 20, 40, … 7000 in a tight loop, for an input
//     of exactly 7000 bytes.
//   * `tool-progress-unruly` — progress for a call never started, a start with an empty id, the
//     same start twice, then a count and a smaller one, before returning `c1` with `INPUT`.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "driver",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use murmur::stream::events;

/// What the call's input carries, so a test can tell it apart from everything else on the wire.
const MARKER: &str = "progress-marker-7f3a";
const CALL_ID: &str = "c1";
const TOOL: &str = "echo-tool";
/// The size `tool-progress-large`'s input serializes to, and the last count it reports.
const LARGE_INPUT_BYTES: u64 = 7000;

/// The call's input: one key whose value holds the marker, padded with `x` to `bytes` bytes of
/// compact JSON when `bytes` is given.
fn input(bytes: Option<u64>) -> serde_json::Value {
    let mut value = MARKER.to_string();
    if let Some(bytes) = bytes {
        let bare = serde_json::json!({ "msg": "" }).to_string().len() + MARKER.len();
        value.push_str(&"x".repeat(bytes as usize - bare));
    }
    serde_json::json!({ "msg": value })
}

fn size(input: &serde_json::Value) -> u64 {
    input.to_string().len() as u64
}

fn answer(response: serde_json::Value) -> ToolResult {
    ToolResult {
        status: Status::Passed,
        summary: Some("done".to_string()),
        data: Some(response.to_string()),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}

fn call_block(input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "type": "tool_call", "id": CALL_ID, "name": TOOL, "input": input })
}

struct ToolProgressDriver;

impl Guest for ToolProgressDriver {
    fn run(request: ToolInput) -> ToolResult {
        let request = request.data.unwrap_or_default();
        if request.contains(r#""role":"tool""#) {
            return answer(serde_json::json!({
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": "PROGRESS-ANSWER" }]
            }));
        }

        let mode = std::env::var("MURMUR_INFERENCE_MODEL").unwrap_or_default();
        let mut content = Vec::new();
        let call_input = match mode.as_str() {
            "tool-progress-large" => {
                let input = input(Some(LARGE_INPUT_BYTES));
                events::tool_call_started(CALL_ID, TOOL);
                for i in 1..=LARGE_INPUT_BYTES / 20 {
                    events::tool_call_input_bytes(CALL_ID, i * 20);
                }
                input
            }
            "tool-progress-unruly" => {
                let input = input(None);
                events::tool_call_input_bytes("ghost", 5);
                events::tool_call_started("", TOOL);
                events::tool_call_started(CALL_ID, TOOL);
                events::tool_call_started(CALL_ID, TOOL);
                events::tool_call_input_bytes(CALL_ID, 30);
                events::tool_call_input_bytes(CALL_ID, 10);
                input
            }
            _ => {
                if mode == "tool-progress-streaming" {
                    events::emit_thinking_chunk("planning");
                    events::emit_chunk("Writing it. ");
                    content.push(serde_json::json!({ "type": "text", "text": "Writing it. " }));
                }
                let input = input(None);
                let n = size(&input);
                events::tool_call_started(CALL_ID, TOOL);
                events::tool_call_input_bytes(CALL_ID, n / 2);
                events::tool_call_input_bytes(CALL_ID, n);
                input
            }
        };
        content.push(call_block(call_input));
        answer(serde_json::json!({ "stop_reason": "tool_call", "content": content }))
    }
}

export!(ToolProgressDriver);
