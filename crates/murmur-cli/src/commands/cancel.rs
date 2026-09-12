use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};
use crate::live_address::Target;
use crate::residue::{parts_from_artifacts, print_residue};

/// How long the door has to accept a connection. A capsule mid-turn accepts immediately — the
/// accept loop and the agent loop are different tasks — so anything slower is an address nothing
/// is listening on.
const DOOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the door has to answer once connected. Generous: the method itself takes a lock and
/// answers, but it does so behind whatever else the connection task is already serving.
const DOOR_READ_TIMEOUT: Duration = Duration::from_secs(30);

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
    print_residue(&parts_from_artifacts(result));

    Ok(())
}

/// One JSON-RPC POST to the capsule's door, parsed.
///
/// Shared with `mur stop`, which asks the same door a different method. Both deadlines are held
/// here rather than at either caller: a door that accepts a connection and then says nothing
/// would otherwise leave the command waiting on it forever, and `mur stop` has two more steps to
/// run whatever the door does.
pub(crate) fn post_json(addr: &str, body: &str) -> Result<Value, CliError> {
    let stream = connect_with_timeout(addr)?;
    stream.set_read_timeout(Some(DOOR_READ_TIMEOUT)).ok();
    let mut writer = &stream;

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    writer
        .write_all(request.as_bytes())
        .map_err(|e| CliError::new(E_IO_003, format!("failed to send request: {e}")))?;
    writer
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

/// Connect to `addr` under [`DOOR_CONNECT_TIMEOUT`].
///
/// `TcpStream::connect_timeout` takes a resolved `SocketAddr`, so the host is resolved first and
/// every address it yields is tried in turn, which is what the plain `connect` does for free.
fn connect_with_timeout(addr: &str) -> Result<TcpStream, CliError> {
    let resolved: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| CliError::new(E_IO_003, format!("failed to resolve {addr}: {e}")))?
        .collect();
    let mut last = None;
    for socket_addr in resolved {
        match TcpStream::connect_timeout(&socket_addr, DOOR_CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(CliError::new(
        E_IO_003,
        match last {
            Some(e) => format!("failed to connect to {addr}: {e}"),
            None => format!("failed to connect to {addr}: it resolved to no address"),
        },
    ))
}
