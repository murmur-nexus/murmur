// A tool that drives `murmur:process` one operation at a time, so a test can see each rule the
// runtime applies. The input is `{"op": …}`, or `{"op":"seq","steps":[…]}` for several in one
// call; the data is JSON: the operation's output, or `{"error":{"kind","rule","reason"}}`.
//
// Ops: `run` (`program`, `args`, `env`, `stdin` or `stdin_repeat`, `timeout_ms`, `quiet`), `start` (`name`, `program`,
// `args`, `env`), `find` (`name`), `status`, `write` (`data`), `read` (`max`), `wait_output`
// (`contains`, `timeout_ms`), `stop`, and `connect` (`port`, `send`, `timeout_ms`). The ops after
// `start` and `find` take `handle`, or `name` to find it first.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use murmur::process::process as process;
use serde_json::{json, Value};

struct ProcessProbe;

impl exports::murmur::tool::run::Guest for ProcessProbe {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let request: Value =
            serde_json::from_str(&input.data.unwrap_or_default()).unwrap_or(Value::Null);
        let data = if request["op"] == "seq" {
            Value::Array(
                request["steps"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(op)
                    .collect(),
            )
        } else {
            op(&request)
        };
        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some("process-probe".to_string()),
            data: Some(data.to_string()),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
            attachments: Vec::new(),
        }
    }
}

fn error_json(error: process::ProcessError) -> Value {
    let (kind, rule, reason) = match error {
        process::ProcessError::Refused(refusal) => ("refused", refusal.rule, refusal.reason),
        process::ProcessError::UnknownHandle => ("unknown-handle", String::new(), String::new()),
        process::ProcessError::NotRunning => ("not-running", String::new(), String::new()),
        process::ProcessError::TimedOut => ("timed-out", String::new(), String::new()),
        process::ProcessError::Failed(text) => ("failed", String::new(), text),
    };
    json!({"error": {"kind": kind, "rule": rule, "reason": reason}})
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn env(value: &Value) -> Vec<(String, String)> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn state_json(state: process::State) -> Value {
    match state {
        process::State::Running => json!({"state": "running"}),
        process::State::Exited(code) => json!({"state": "exited", "exit_code": code}),
    }
}

/// `request["handle"]`, or the handle `find` gives for `request["name"]`.
fn handle_of(request: &Value) -> Result<String, Value> {
    if let Some(handle) = request["handle"].as_str() {
        return Ok(handle.to_string());
    }
    let name = request["name"].as_str().unwrap_or_default();
    process::find(name).ok_or_else(|| json!({"error": {"kind": "not-found", "rule": "", "reason": name}}))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn op(request: &Value) -> Value {
    match request["op"].as_str().unwrap_or_default() {
        "run" => {
            let stdin = request["stdin"].as_str().map(|s| s.as_bytes().to_vec()).or_else(|| {
                request["stdin_repeat"].as_u64().map(|count| vec![b'x'; count as usize])
            });
            match process::run(&process::RunRequest {
                program: request["program"].as_str().unwrap_or_default().to_string(),
                args: strings(&request["args"]),
                env: env(&request["env"]),
                stdin,
                timeout_ms: request["timeout_ms"].as_u64(),
            }) {
                // `quiet` reports the lengths without the text, for output too large to hand
                // back to a model.
                Ok(output) if request["quiet"] == true => json!({
                    "exit_code": output.exit_code,
                    "timed_out": output.timed_out,
                    "stdout_len": output.stdout.len(),
                    "stdout_truncated": output.stdout_truncated,
                    "stderr_len": output.stderr.len(),
                    "stderr_truncated": output.stderr_truncated,
                }),
                Ok(output) => json!({
                    "exit_code": output.exit_code,
                    "timed_out": output.timed_out,
                    "stdout": text(&output.stdout),
                    "stdout_len": output.stdout.len(),
                    "stdout_truncated": output.stdout_truncated,
                    "stderr": text(&output.stderr),
                    "stderr_truncated": output.stderr_truncated,
                }),
                Err(error) => error_json(error),
            }
        }
        "start" => match process::start(&process::StartRequest {
            name: request["name"].as_str().unwrap_or_default().to_string(),
            program: request["program"].as_str().unwrap_or_default().to_string(),
            args: strings(&request["args"]),
            env: env(&request["env"]),
        }) {
            Ok(started) => json!({"handle": started.handle, "already_running": started.already_running}),
            Err(error) => error_json(error),
        },
        "find" => json!({"handle": process::find(request["name"].as_str().unwrap_or_default())}),
        "status" => match handle_of(request) {
            Ok(handle) => process::status(&handle).map_or_else(error_json, state_json),
            Err(error) => error,
        },
        "write" => match handle_of(request) {
            Ok(handle) => {
                let data = request["data"].as_str().unwrap_or_default();
                process::write(&handle, data.as_bytes())
                    .map_or_else(error_json, |()| json!({"written": data.len()}))
            }
            Err(error) => error,
        },
        "read" => match handle_of(request) {
            Ok(handle) => {
                let max = request["max"].as_u64().unwrap_or(1 << 20) as u32;
                match process::read(&handle, max) {
                    Ok(output) => json!({
                        "stdout": text(&output.stdout),
                        "stderr": text(&output.stderr),
                        "stdout_dropped": output.stdout_dropped,
                        "stderr_dropped": output.stderr_dropped,
                    }),
                    Err(error) => error_json(error),
                }
            }
            Err(error) => error,
        },
        "wait_output" => match handle_of(request) {
            Ok(handle) => {
                let wanted = request["contains"].as_str().unwrap_or_default();
                let timeout = request["timeout_ms"].as_u64().unwrap_or(10_000);
                let mut seen = String::new();
                let mut waited = 0u64;
                loop {
                    match process::read(&handle, 1 << 20) {
                        Ok(output) => {
                            seen.push_str(&text(&output.stdout));
                            seen.push_str(&text(&output.stderr));
                        }
                        Err(error) => break error_json(error),
                    }
                    if seen.contains(wanted) {
                        break json!({"found": true, "output": seen});
                    }
                    if waited >= timeout {
                        break json!({"found": false, "output": seen});
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    waited += 50;
                }
            }
            Err(error) => error,
        },
        "stop" => match handle_of(request) {
            Ok(handle) => process::stop(&handle).map_or_else(error_json, state_json),
            Err(error) => error,
        },
        "connect" => match handle_of(request) {
            Ok(handle) => {
                let port = request["port"].as_u64().unwrap_or(0) as u16;
                let timeout = request["timeout_ms"].as_u64().unwrap_or(5_000) as u32;
                match process::connect(&handle, port) {
                    Ok(connection) => {
                        let send = request["send"].as_str().unwrap_or_default();
                        if let Err(error) = connection.write(send.as_bytes()) {
                            return error_json(error);
                        }
                        let mut received = Vec::new();
                        loop {
                            match connection.read(65536, timeout) {
                                Ok(bytes) if bytes.is_empty() => break,
                                Ok(bytes) => received.extend(bytes),
                                Err(process::ProcessError::TimedOut) => break,
                                Err(error) => return error_json(error),
                            }
                        }
                        json!({"response": text(&received)})
                    }
                    Err(error) => error_json(error),
                }
            }
            Err(error) => error,
        },
        other => json!({"error": {"kind": "unknown-op", "rule": "", "reason": other}}),
    }
}

export!(ProcessProbe);
