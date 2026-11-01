// Test-only tool-world component that waits on the host and never sends anything: its `run`
// sleeps 600 seconds — a `wasi:clocks` wait, during which no guest code runs — before it returns
// `passed` with no data. Installed as an inference driver it is a driver that stalls before
// sending its request; installed as a tool, a tool call that never comes back.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};

struct StallDriver;

impl Guest for StallDriver {
    fn run(_input: ToolInput) -> ToolResult {
        std::thread::sleep(std::time::Duration::from_secs(600));
        ToolResult {
            status: Status::Passed,
            summary: None,
            data: None,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

export!(StallDriver);
