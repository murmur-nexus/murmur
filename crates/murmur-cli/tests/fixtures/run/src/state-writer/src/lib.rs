// A tool that exercises the `state` preopen the way a real artifact would: one append per
// invocation, then a read-back of everything previous invocations left. The line count it
// returns is what makes durability observable from outside the guest — a fresh session workdir
// on every launch means the count can only grow if the store itself survived.
//
// Failures are reported rather than trapped, so the default-deny case (no `capabilities.state`,
// so no `state` preopen) is a readable outcome instead of an indistinguishable crash.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use std::io::Write;

const NOTES: &str = "state/notes.jsonl";

struct StateWriter;

impl exports::murmur::tool::run::Guest for StateWriter {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let summary = match append_and_count(input.data.as_deref().unwrap_or("note")) {
            Ok(lines) => lines.to_string(),
            Err(message) => format!("state-denied: {message}"),
        };

        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some(summary),
            data: None,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

/// Append one line to the store, then report how many lines are readable there.
fn append_and_count(note: &str) -> Result<usize, String> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(NOTES)
        .map_err(|err| err.to_string())?;
    writeln!(file, "{{\"note\":\"{note}\"}}").map_err(|err| err.to_string())?;
    drop(file);

    let contents = std::fs::read_to_string(NOTES).map_err(|err| err.to_string())?;
    Ok(contents.lines().filter(|line| !line.is_empty()).count())
}

export!(StateWriter);
