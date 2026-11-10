//! The escape-conformance probe guest: hands its input back to its host as an `escape-probe`
//! request.
//!
//! The gate's `wasm-guest` route measures a guest worker as if a guest had escaped Wasmtime and
//! were making raw host syscalls. A guest cannot make one, so this guest asks its host side to
//! make them: it returns its input verbatim under the result metadata key
//! `murmur-escape-probe`, and a guest worker built with the test-only `escape-probe` Cargo
//! feature (`capsule-runtime/src/guest_worker/escape_probe.rs`) runs the probe body the request
//! names in the worker process, under the worker's enforcement, writes the verdict file into the
//! work folder and returns the verdict as the result. Any other build ignores the key, so the
//! result reads `escape-probe request not answered`, which the gate grades `INCONCLUSIVE`.
wit_bindgen::generate!({
    path: "../../wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

/// The result metadata key a guest worker built with `escape-probe` answers.
const REQUEST_KEY: &str = "murmur-escape-probe";

struct ProbeGuest;

impl Guest for ProbeGuest {
    fn run(input: ToolInput) -> ToolResult {
        ToolResult {
            status: Status::Passed,
            summary: Some("escape-probe request not answered".to_string()),
            data: None,
            data_path: None,
            truncated: false,
            metadata: vec![(REQUEST_KEY.to_string(), input.data.unwrap_or_default())],
            attachments: Vec::new(),
        }
    }
}

export!(ProbeGuest);
