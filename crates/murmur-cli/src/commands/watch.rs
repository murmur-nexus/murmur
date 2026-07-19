use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};

/// Connect to a capsule's SSE observer endpoint (`stream/watch`) and print events to stdout.
///
/// Unlike `message/stream`, this does not submit a task. It passively observes the capsule's
/// SSE stream, including any events buffered since the capsule started. The process stays
/// connected across task turns and exits only when the capsule closes or Ctrl+C is pressed.
pub(crate) fn run_watch(capsule_url: &str) -> Result<(), CliError> {
    let addr = capsule_url
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    let mut stream = TcpStream::connect(addr)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to connect to {addr}: {e}")))?;

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "stream/watch",
        "params": {}
    })
    .to_string();

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nLast-Event-ID: 0\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        body.len(),
        body
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| CliError::new(E_IO_003, format!("failed to send request: {e}")))?;
    stream
        .flush()
        .map_err(|e| CliError::new(E_IO_003, format!("failed to flush request: {e}")))?;

    let mut reader = BufReader::new(&stream);

    // Read and discard HTTP response headers; check status line
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read response status: {e}")))?;

    if !status_line.contains("200") {
        return Err(CliError::new(
            E_IO_003,
            format!("capsule returned non-200 response: {}", status_line.trim()),
        ));
    }

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
    }

    // SSE stream state
    let mut conversation_mode = String::from("stateless");
    let mut task_context_map: HashMap<String, String> = HashMap::new();
    let mut context_turns: HashMap<String, u32> = HashMap::new();

    // Parse SSE event stream
    let mut current_event_type = String::new();
    let mut current_data = String::new();

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Err(_) => break,
            Ok(_) => {}
        }

        let line = line.trim_end_matches('\n').trim_end_matches('\r').to_string();

        if line.is_empty() {
            // Dispatch the accumulated event
            if !current_event_type.is_empty() && !current_data.is_empty() {
                match current_event_type.as_str() {
                    "connection-ack" => {
                        if let Ok(event) = serde_json::from_str::<Value>(&current_data) {
                            if let Some(mode) =
                                event.get("conversation_mode").and_then(Value::as_str)
                            {
                                conversation_mode = mode.to_string();
                            }
                        }
                    }
                    "gap" => {
                        let first_id = serde_json::from_str::<Value>(&current_data)
                            .ok()
                            .and_then(|v| v.get("first_available_id").and_then(Value::as_u64))
                            .unwrap_or(0);
                        eprintln!("[murmur] warning: buffer overflow — some earlier events were lost (first available id: {first_id})");
                    }
                    "capsule-closed" => {
                        eprintln!("[murmur] capsule closed");
                        return Ok(());
                    }
                    _ => {
                        dispatch_sse_event(
                            &current_event_type,
                            &current_data,
                            &conversation_mode,
                            &mut task_context_map,
                            &mut context_turns,
                        );
                        // Do NOT exit on final — stream/watch persists across task turns.
                    }
                }
            }
            current_event_type.clear();
            current_data.clear();
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            current_data = rest.to_string();
        } else if line.starts_with(':') {
            // SSE comment (heartbeat) — ignore
        } else if let Some(rest) = line.strip_prefix("id: ") {
            // Event ID — tracked by client for reconnection; not used in watch
            let _ = rest;
        }
    }

    Ok(())
}

/// Print the event and return true if it was a final event.
fn dispatch_sse_event(
    event_type: &str,
    data: &str,
    conversation_mode: &str,
    task_context_map: &mut HashMap<String, String>,
    context_turns: &mut HashMap<String, u32>,
) -> bool {
    let is_final = data.contains("\"final\":true");
    let is_threaded = conversation_mode == "threaded";

    match event_type {
        "status" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                let task_id =
                    event.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let context_id_opt =
                    event.get("context_id").and_then(Value::as_str).map(str::to_string);
                let state = event
                    .get("status")
                    .and_then(|s| s.get("state"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let message = event
                    .get("status")
                    .and_then(|s| s.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let response = event
                    .get("status")
                    .and_then(|s| s.get("response"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                // Update task→context mapping
                if let Some(ref cid) = context_id_opt {
                    task_context_map.insert(task_id.clone(), cid.clone());
                }

                if is_threaded {
                    let cid = context_id_opt
                        .or_else(|| task_context_map.get(&task_id).cloned())
                        .unwrap_or_else(|| task_id.clone());

                    let turn = *context_turns.entry(cid.clone()).or_insert(1);
                    let prefix =
                        format!("[{} / turn {turn} / {state}]", truncate_context_id(&cid));

                    if is_final {
                        println!("{prefix}");
                        if state == "completed" {
                            if let Some(ref resp) = response {
                                let col_width = terminal_width().saturating_sub(2);
                                for line in wrap_text(resp, col_width) {
                                    println!("  {line}");
                                }
                                println!();
                            }
                        }
                        *context_turns.entry(cid).or_insert(1) += 1;
                    } else {
                        println!("{prefix}  {message}");
                    }
                } else {
                    if is_final {
                        println!("[{state}]");
                        if state == "completed" {
                            if let Some(ref resp) = response {
                                let col_width = terminal_width().saturating_sub(2);
                                for line in wrap_text(resp, col_width) {
                                    println!("  {line}");
                                }
                                println!();
                            }
                        }
                    } else {
                        println!("[{state}]  {message}");
                    }
                }
            }
        }
        "artifact" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                let tool_name = event
                    .get("artifact")
                    .and_then(|a| a.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let content = event
                    .get("artifact")
                    .and_then(|a| a.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut content_lines = content.lines();
                if let Some(first) = content_lines.next() {
                    println!("[artifact] tool: {tool_name} | {first}");
                    for rest in content_lines {
                        println!("  {rest}");
                    }
                } else {
                    println!("[artifact] tool: {tool_name}");
                }
            }
        }
        "text" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        print!("{text}");
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        }
        _ => {}
    }

    is_final
}

/// Truncate a context ID for display: "ctx_XXXXXXXX..." (first 8 hex chars after the prefix).
fn truncate_context_id(ctx_id: &str) -> String {
    if let Some(rest) = ctx_id.strip_prefix("ctx_") {
        let truncated: String = rest.chars().take(8).collect();
        format!("ctx_{truncated}...")
    } else {
        format!("{:.12}...", ctx_id)
    }
}

fn terminal_width() -> usize {
    80
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}
