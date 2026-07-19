// Test-only inference driver that exercises the stateful-driver continuation protocol.
// It is intentionally provider-agnostic: it speaks the host's own driver
// request/response JSON shape directly and never makes an HTTP call.
//
// Behavior, decided purely from the incoming wire payload (drivers are re-instantiated per
// dispatch, so no in-process state persists across Turns):
//
//   * Turn 0 — a full, single user message and no request-side `continuation_id` — returns a
//     `bash` tool_call so the loop advances to a second Turn. When the driver config env var
//     `MURMUR_INFERENCE_DRIVER_CONFIG` is `NOCONT` the driver leaves `metadata` empty
//     (mimicking every driver shipped today → full resend every Turn); otherwise it returns
//     `metadata = [("continuation_id", "cont-echo-1")]` to opt into incremental resend.
//     Opt-in is keyed off the config env var (not the task text) so two sessions can run
//     byte-identical logical content and still differ only in continuation behavior — the
//     `MURMUR_INFERENCE_DRIVER_CONFIG` value never appears in the driver request payload.
//
//   * Any later Turn — ends the Task, echoing what it actually saw on the wire into the final
//     text as `wire_n=<message count> cont=<continuation id or "none">`, which the host writes
//     to `out/result.txt`. A test reads that file to confirm incremental-vs-full directly,
//     without reading the host source.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

struct ContinuationDriver;

impl Guest for ContinuationDriver {
    fn run(input: ToolInput) -> ToolResult {
        let data = input.data.unwrap_or_default();
        let payload: serde_json::Value =
            serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);

        let messages = payload.get("messages").and_then(|m| m.as_array());
        let wire_n = messages.map(|m| m.len()).unwrap_or(0);
        let request_continuation = payload.get("continuation_id").and_then(|c| c.as_str());

        // Turn 0: the host resends the full transcript (one user message) and has no
        // continuation to echo back yet.
        let is_turn_zero = request_continuation.is_none() && wire_n <= 1;
        if is_turn_zero {
            let opt_into_continuation = std::env::var("MURMUR_INFERENCE_DRIVER_CONFIG")
                .map(|v| !v.contains("NOCONT"))
                .unwrap_or(true);

            let response = serde_json::json!({
                "stop_reason": "tool_call",
                "content": [{
                    "type": "tool_call",
                    "id": "toolu_echo",
                    "name": "bash",
                    "input": {"command": "echo hello"}
                }]
            })
            .to_string();

            let metadata = if opt_into_continuation {
                vec![("continuation_id".to_string(), "cont-echo-1".to_string())]
            } else {
                Vec::new()
            };

            return ToolResult {
                status: Status::Passed,
                summary: Some("turn0".to_string()),
                data: Some(response),
                data_path: None,
                truncated: false,
                metadata,
            };
        }

        // Later Turn: report the wire payload the host actually transmitted, then end the Task.
        let report = format!("wire_n={} cont={}", wire_n, request_continuation.unwrap_or("none"));
        let response = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": report}]
        })
        .to_string();

        ToolResult {
            status: Status::Passed,
            summary: Some("done".to_string()),
            data: Some(response),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(ContinuationDriver);
