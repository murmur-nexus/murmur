// Test-only inference driver that exercises the optional `usage` block of the driver response
// contract. It is provider-agnostic: it speaks the host's own driver request/response JSON
// shape directly and never makes an HTTP call.
//
// Every dispatch ends the Task on Turn 0 with `stop_reason: "end_turn"`, so a session runs
// exactly one inference turn and the trace holds exactly one `inference` event to read. What
// rides alongside that response is selected by `MURMUR_INFERENCE_DRIVER_CONFIG`, which the
// manifest supplies through `inference.driver.config`:
//
//   * unset (or anything unrecognized) — a well-formed `usage` block with all four members at
//     the constants below. The values are constants so a test can assert them verbatim, and
//     `INPUT_TOKENS` is far from any tiktoken count of this payload so the estimate and the
//     actual are visibly different numbers.
//   * `NOUSAGE` — no `usage` member at all: the shape a driver that reports nothing returns.
//   * `BADUSAGE` — a `usage` whose members are all ill-typed (a string, a negative, an unknown
//     member), which the host must degrade to "absent" rather than reject.
//   * `NONOBJECTUSAGE` — a `usage` that is not an object at all.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

/// Reported as `usage.input_tokens`. Deliberately nowhere near a tiktoken count of the
/// payload this fixture is sent, so a test can tell the two apart without arithmetic.
const INPUT_TOKENS: u64 = 12043;
const OUTPUT_TOKENS: u64 = 218;
const CACHED_TOKENS: u64 = 11780;
const CACHE_WRITE_TOKENS: u64 = 7;

struct UsageDriver;

impl Guest for UsageDriver {
    fn run(_input: ToolInput) -> ToolResult {
        let mode = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG").unwrap_or_default();

        let mut response = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "usage fixture done"}]
        });

        if mode.contains("BADUSAGE") {
            response["usage"] = serde_json::json!({
                "input_tokens": "12043",
                "cached_tokens": -4,
                "unknown_member": 7
            });
        } else if mode.contains("NONOBJECTUSAGE") {
            response["usage"] = serde_json::json!("12043");
        } else if !mode.contains("NOUSAGE") {
            response["usage"] = serde_json::json!({
                "input_tokens": INPUT_TOKENS,
                "output_tokens": OUTPUT_TOKENS,
                "cached_tokens": CACHED_TOKENS,
                "cache_write_tokens": CACHE_WRITE_TOKENS
            });
        }

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

export!(UsageDriver);
