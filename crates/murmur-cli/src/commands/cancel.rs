use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};
use crate::live_address::Target;

/// Stop one running task on a capsule without ending its session.
///
/// A thin caller of the A2A door's `tasks/cancel`: it composes one JSON-RPC request, prints what
/// came back, and decides nothing. What a cancel means — which state the task lands in, what is
/// left running — is the runtime's answer, reproduced here.
///
/// Exits 0 for every task the capsule holds, including one that had already ended: "do no more
/// work on this" is already true of a completed task, so there is nothing to report as a failure.
/// A task id the capsule never held is the one error.
pub(crate) fn run_cancel(target: &Target, task_id: &str) -> Result<(), CliError> {
    let addr = target
        .url()
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tasks/cancel",
        "params": {"id": task_id}
    })
    .to_string();

    let response = post_json(addr, &body)?;

    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the capsule refused the cancel");
        return Err(CliError::new(
            E_IO_003,
            format!("cancel of {task_id} failed: {message}"),
        ));
    }

    let result = response.get("result").ok_or_else(|| {
        CliError::new(
            E_IO_003,
            format!("the capsule answered {task_id} with neither a result nor an error"),
        )
    })?;

    let state = result
        .get("status")
        .and_then(|status| status.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let id = result.get("id").and_then(Value::as_str).unwrap_or(task_id);
    println!("task:    {id}");
    println!("state:   {state}");

    // One line per thing the capsule left running. Nothing was killed: a detached command keeps
    // its own lifecycle and a delegated child is still going, and both are named so whoever
    // cancelled knows what is still out there.
    let residue: Vec<&Value> = result
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|artifacts| {
            artifacts
                .iter()
                .filter(|artifact| artifact.get("name").and_then(Value::as_str) == Some("residue"))
                .filter_map(|artifact| artifact.get("parts").and_then(Value::as_array))
                .flatten()
                .collect()
        })
        .unwrap_or_default();

    for part in residue {
        let Some(item) = part
            .get("text")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
        else {
            continue;
        };
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "detached_shell" => println!(
                "running: {}  detached shell  {}",
                item.get("work_id").and_then(Value::as_str).unwrap_or("?"),
                item.get("command").and_then(Value::as_str).unwrap_or("")
            ),
            "delegation" => println!(
                "running: {}  delegation  {}@{}",
                item.get("delegation_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                item.get("capsule").and_then(Value::as_str).unwrap_or("?"),
                item.get("version").and_then(Value::as_str).unwrap_or("?")
            ),
            other => println!("running: {other}"),
        }
    }

    Ok(())
}

/// One JSON-RPC POST to the capsule's door, parsed.
fn post_json(addr: &str, body: &str) -> Result<Value, CliError> {
    let mut stream = TcpStream::connect(addr)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to connect to {addr}: {e}")))?;

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| CliError::new(E_IO_003, format!("failed to send request: {e}")))?;
    stream
        .flush()
        .map_err(|e| CliError::new(E_IO_003, format!("failed to flush request: {e}")))?;

    let mut reader = BufReader::new(&stream);
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
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
    }

    let mut response_body = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        response_body.push_str(&line);
    }

    serde_json::from_str(&response_body).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("the capsule's reply was not JSON ({e}): {response_body}"),
        )
    })
}
