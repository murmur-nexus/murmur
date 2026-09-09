// Test-only inference driver that scripts each `stop_reason` the agent loop dispatches on. It is
// provider-agnostic: it speaks the host's own driver request/response JSON shape directly and
// never makes an HTTP call.
//
// Which response it returns is selected by `MURMUR_INFERENCE_DRIVER_CONFIG`, which the manifest
// supplies through `inference.driver.config`:
//
//   * unset (or anything unrecognized) — `stop_reason: "max_tokens"` with a partial sentence,
//     the shape a provider returns for a turn it cut off at the output cap.
//   * `ENDTURN` — `stop_reason: "end_turn"` with the same text, so the capped and the completed
//     turn differ in nothing but the stop reason.
//   * `EMPTY` — `stop_reason: "max_tokens"` with a text block holding the empty string: a turn
//     cut off before it produced any text at all.
//   * `NOSTOP` — a response carrying no `stop_reason` member, which the loop dispatches on as
//     the empty string.
//   * `TOOLTHENCAP` — two turns. Turn 0 asks for a tool the capsule does not install, which the
//     dispatcher refuses and the loop carries past; turn 1 answers `stop_reason: "max_tokens"`.
//     The turns are told apart by the request payload, which already carries a `"role":"tool"`
//     message on the second call.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

/// The fragment a capped turn returns. Ends mid-sentence, with no trailing whitespace, so a test
/// can assert `out/result.txt` byte-for-byte against it and against it plus a marker.
const PARTIAL_TEXT: &str = "The three main causes are, first, the";

/// The tool turn 0 asks for under `TOOLTHENCAP`. No capsule here installs it, so the dispatcher
/// refuses the call and the loop continues to turn 1 with the refusal as a tool result.
const ABSENT_TOOL: &str = "not-installed-tool";

struct TruncationDriver;

impl Guest for TruncationDriver {
    fn run(input: ToolInput) -> ToolResult {
        let mode = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").unwrap_or_default();
        let request = input.data.unwrap_or_default();

        let response = if mode.contains("ENDTURN") {
            serde_json::json!({
                "stop_reason": "end_turn",
                "content": [{"type": "text", "text": PARTIAL_TEXT}]
            })
        } else if mode.contains("EMPTY") {
            serde_json::json!({
                "stop_reason": "max_tokens",
                "content": [{"type": "text", "text": ""}]
            })
        } else if mode.contains("NOSTOP") {
            serde_json::json!({
                "content": [{"type": "text", "text": PARTIAL_TEXT}]
            })
        } else if mode.contains("TOOLTHENCAP") && !request.contains("\"role\":\"tool\"") {
            serde_json::json!({
                "stop_reason": "tool_call",
                "content": [{
                    "type": "tool_call",
                    "id": "call_0",
                    "name": ABSENT_TOOL,
                    "input": {}
                }]
            })
        } else {
            serde_json::json!({
                "stop_reason": "max_tokens",
                "content": [{"type": "text", "text": PARTIAL_TEXT}]
            })
        };

        ToolResult {
            status: Status::Passed,
            summary: Some("done".to_string()),
            data: Some(response.to_string()),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(TruncationDriver);
