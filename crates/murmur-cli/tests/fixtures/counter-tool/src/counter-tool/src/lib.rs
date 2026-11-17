// Test-only tool that keeps a counter in its instance's memory, for the code mode tests: a fresh
// instance per call answers `count=1` every time, a session-long one counts up.
//
// Its input is a JSON object; `op` picks what it does:
//
//   incr      add one to the counter and answer `count=<n>`
//   sleep     sleep `secs` seconds (a `wasi:clocks` wait), then add one and answer
//   spin      loop with no host call until the instance is stopped or interrupted
//   trap      panic, which traps the instance
//   grow      allocate and hold `mib` MiB, then add one and answer
//
// Every answer has status `passed`.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use serde_json::Value;

static COUNT: AtomicU64 = AtomicU64::new(0);

/// Allocations `grow` holds for the rest of the instance's life.
static HELD: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

fn answer() -> ToolResult {
    let count = COUNT.fetch_add(1, Ordering::SeqCst) + 1;
    ToolResult {
        status: Status::Passed,
        summary: None,
        data: Some(format!("count={count}")),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

struct CounterTool;

impl Guest for CounterTool {
    fn run(input: ToolInput) -> ToolResult {
        let input: Value =
            serde_json::from_str(input.data.as_deref().unwrap_or("{}")).unwrap_or(Value::Null);
        let number = |key: &str| input.get(key).and_then(Value::as_u64).unwrap_or(0);
        match input.get("op").and_then(Value::as_str).unwrap_or("incr") {
            "sleep" => {
                std::thread::sleep(std::time::Duration::from_secs(number("secs")));
                answer()
            }
            "spin" => {
                let mut turns: u64 = 0;
                loop {
                    turns = std::hint::black_box(turns.wrapping_add(1));
                }
            }
            "trap" => panic!("counter-tool was asked to trap"),
            "grow" => {
                let bytes = usize::try_from(number("mib")).unwrap_or(usize::MAX) * 1024 * 1024;
                let held = vec![1u8; bytes];
                HELD.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(held);
                answer()
            }
            _ => answer(),
        }
    }
}

export!(CounterTool);
