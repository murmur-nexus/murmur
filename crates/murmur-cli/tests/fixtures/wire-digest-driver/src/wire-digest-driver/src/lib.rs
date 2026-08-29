// Test-only inference driver that hashes the request it was handed and reports the digests.
// It is provider-agnostic: it speaks the host's own driver request/response JSON shape directly
// and never makes an HTTP call.
//
// It ends the task on turn 0, so a session runs exactly one inference turn and the capsule needs
// no shell capability. Its final text is
//
//   system=<sha256> tools=<sha256> messages=<sha256>,<sha256>,...
//
// hashed from the payload *as this guest received it* — the system string's UTF-8 bytes, the
// serialized `tools` array, and each element of the `messages` array. The host writes that text
// to `out/result.txt`, so a test can compare the driver's own view of the wire against the
// `system_sha`/`tools_sha`/`message_shas` the trace recorded, without either side trusting the
// other's serializer.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

struct WireDigestDriver;

impl Guest for WireDigestDriver {
    fn run(input: ToolInput) -> ToolResult {
        let data = input.data.unwrap_or_default();
        let payload: serde_json::Value =
            serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);

        let system = sha256_hex(
            payload
                .get("system")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .as_bytes(),
        );
        let tools = sha256_hex(
            &payload
                .get("tools")
                .map(|t| serde_json::to_vec(t).unwrap_or_default())
                .unwrap_or_default(),
        );
        let messages: Vec<String> = payload
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|messages| {
                messages
                    .iter()
                    .map(|message| sha256_hex(&serde_json::to_vec(message).unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();

        let report = format!(
            "system={} tools={} messages={}",
            system,
            tools,
            messages.join(",")
        );
        let response = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": report}]
        })
        .to_string();

        ToolResult {
            status: Status::Passed,
            summary: Some("digested".to_string()),
            data: Some(response),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(WireDigestDriver);
