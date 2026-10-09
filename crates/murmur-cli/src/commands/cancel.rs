use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};
use crate::live_address::Target;
use crate::residue::{parts_from_artifacts, residue_lines};

/// How long the door has to accept a connection. A capsule mid-turn accepts immediately — the
/// accept loop and the agent loop are different tasks — so anything slower is an address nothing
/// is listening on.
const DOOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the door has to answer once connected. Generous: the method itself takes a lock and
/// answers, but it does so behind whatever else the connection task is already serving.
const DOOR_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Stop one running task on a capsule without ending its session.
///
/// A thin caller of the A2A door's `CancelTask`: it composes one JSON-RPC request, prints what
/// came back, and decides nothing. What a cancel means — which state the task lands in, what is
/// left running — is the runtime's answer, reproduced here.
///
/// Exits 0 for every task the capsule holds, including one that had already ended: "do no more
/// work on this" is already true of a completed task, so there is nothing to report as a failure.
/// The door answers such a task `TaskNotCancelable`, whose `ErrorInfo` names the task and the
/// state it ended in, and those are printed in place of a result. A task id the capsule never held
/// is the one error.
pub(crate) fn run_cancel(target: &Target, task_id: &str) -> Result<(), CliError> {
    let addr = target
        .url()
        .trim_start_matches("http://")
        .trim_start_matches("https://");

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "CancelTask",
        "params": {"id": task_id}
    })
    .to_string();

    let response = post_json(addr, &body, target.door_token().as_ref())?;
    for line in cancel_report(task_id, &response)? {
        capsule_runtime::report_println!("{line}");
    }
    Ok(())
}

/// The lines `mur cancel` prints for the door's `response` to cancelling `task_id`, or the error
/// it fails with. Every state is printed in murmur's word.
fn cancel_report(task_id: &str, response: &Value) -> Result<Vec<String>, CliError> {
    if let Some(error) = response.get("error") {
        if let Some((id, state)) = already_ended(error) {
            return Ok(vec![
                format!("task:    {id}"),
                format!("state:   {}", state_line(state)),
                "nothing to cancel: the task had already ended".to_string(),
            ]);
        }
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
    let mut lines = vec![
        format!("task:    {id}"),
        format!("state:   {}", state_line(state)),
    ];

    // One line per thing the capsule left running. Nothing was killed: a detached command keeps
    // its own lifecycle and a delegated child is still going, and both are named so whoever
    // cancelled knows what is still out there.
    lines.extend(residue_lines(&parts_from_artifacts(result)));
    Ok(lines)
}

/// The `state:` line's value for the A2A task state `wire`: murmur's word for it, or `wire` as
/// sent when it is no v1.0 state.
fn state_line(wire: &str) -> &str {
    capsule_runtime::murmur_state_name(wire).unwrap_or(wire)
}

/// The task and the state it ended in, from a JSON-RPC `error` that is `TaskNotCancelable`: the
/// `metadata` of its `google.rpc.ErrorInfo`, whose state is spelled as `GetTask` spells it.
/// `None` for any other error.
fn already_ended(error: &Value) -> Option<(&str, &str)> {
    let code = error.get("code").and_then(Value::as_i64)?;
    if capsule_runtime::A2aError::from_code(i32::try_from(code).ok()?)
        != Some(capsule_runtime::A2aError::TaskNotCancelable)
    {
        return None;
    }
    let metadata = error
        .get("data")?
        .as_array()?
        .iter()
        .find(|detail| detail.get("reason").and_then(Value::as_str) == Some("TASK_NOT_CANCELABLE"))?
        .get("metadata")?;
    Some((
        metadata.get("taskId")?.as_str()?,
        metadata.get("state")?.as_str()?,
    ))
}

/// One JSON-RPC POST to the capsule's door, parsed.
///
/// Shared with `mur stop`, which asks the same door a different method. Both deadlines are held
/// here rather than at either caller: a door that accepts a connection and then says nothing
/// would otherwise leave the command waiting on it forever, and `mur stop` has two more steps to
/// run whatever the door does.
///
/// Every request names [`capsule_runtime::A2A_PROTOCOL_VERSION`] in
/// [`capsule_runtime::A2A_VERSION_HEADER`]. `door_token` is presented as `Authorization: Bearer`
/// when given. A `401` or `403` is an error naming the status and
/// [`capsule_runtime::DOOR_TOKEN_ENV`].
pub(crate) fn post_json(
    addr: &str,
    body: &str,
    door_token: Option<&capsule_runtime::DoorToken>,
) -> Result<Value, CliError> {
    let stream = connect_with_timeout(addr)?;
    stream.set_read_timeout(Some(DOOR_READ_TIMEOUT)).ok();
    let mut writer = &stream;

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\n{}{}: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        authorization_line(door_token),
        capsule_runtime::A2A_VERSION_HEADER,
        capsule_runtime::A2A_PROTOCOL_VERSION,
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
    if let Some(refused) = door_refusal(&status_line) {
        return Err(refused);
    }
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

/// The `Authorization` request-header line presenting `door_token`, or nothing without one.
pub(crate) fn authorization_line(door_token: Option<&capsule_runtime::DoorToken>) -> String {
    door_token
        .map(|token| {
            format!(
                "Authorization: {}\r\n",
                capsule_runtime::bearer_header(token)
            )
        })
        .unwrap_or_default()
}

/// The error for a door that refused the caller's credential, from the response's status line, or
/// `None` for any other status. Names the status, never the token.
pub(crate) fn door_refusal(status_line: &str) -> Option<CliError> {
    let status = status_line.split_whitespace().nth(1)?;
    if status != "401" && status != "403" {
        return None;
    }
    Some(CliError::new(
        E_IO_003,
        format!(
            "the capsule's door refused the call: {} — it declares network.authentication; a \
             session address presents the token in its running record, and --url presents \
             {} when it is set to a token `mur token` printed",
            status_line.trim(),
            capsule_runtime::DOOR_TOKEN_ENV
        ),
    ))
}

/// Connect to `addr` under [`DOOR_CONNECT_TIMEOUT`].
///
/// `TcpStream::connect_timeout` takes a resolved `SocketAddr`, so the host is resolved first and
/// every address it yields is tried in turn, which is what the plain `connect` does for free.
pub(crate) fn connect_with_timeout(addr: &str) -> Result<TcpStream, CliError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    /// A door that answers one request with `answer` and hands back the request it read.
    fn door_answering(answer: Value) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&request).to_string();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|rest| rest.trim().parse().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            stream.read_exact(&mut body).unwrap();
            let answer = answer.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{answer}",
                answer.len()
            )
            .unwrap();
            format!("{head}{}", String::from_utf8_lossy(&body))
        });
        (addr, served)
    }

    fn report(answer: Value) -> Result<Vec<String>, CliError> {
        let (addr, served) = door_answering(answer);
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "CancelTask",
            "params": {"id": "tsk_1"}})
        .to_string();
        let response = post_json(&addr, &body, None).unwrap();
        let request = served.join().unwrap();
        assert!(request.contains("A2A-Version: 1.0\r\n"), "{request}");
        cancel_report("tsk_1", &response)
    }

    /// A cancelled task's state is printed in murmur's word, and the residue lines are read from
    /// the `residue` artifact's data parts.
    #[test]
    fn a_cancel_prints_murmurs_state_and_the_residue_from_data_parts() {
        let lines = report(serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {
        "id": "tsk_1", "contextId": "ctx_1", "status": {"state": "TASK_STATE_CANCELED"},
        "artifacts": [{"artifactId": "residue", "name": "residue", "parts": [
            {"data": {"kind": "detached_shell", "work_id": "wrk_1", "command": "sleep 30"},
             "mediaType": "application/json"},
            {"data": {"kind": "delegation", "delegation_id": "dlg_1", "capsule": "worker",
                      "version": "0.1.0"},
             "mediaType": "application/json"},
        ]}]}}))
        .unwrap();
        assert_eq!(
            lines,
            [
                "task:    tsk_1",
                "state:   canceled",
                "running: wrk_1  detached shell  sleep 30",
                "ended:   dlg_1  delegation  worker@0.1.0",
            ]
        );
    }

    /// A task that had already ended is named with the state the `-32002` metadata carries, in
    /// murmur's word; a state that is no v1.0 state is printed as sent.
    #[test]
    fn an_ended_task_prints_murmurs_word_for_the_state_it_ended_in() {
        let ended = |state: &str| {
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32002,
                "message": "Task cannot be canceled", "data": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "TASK_NOT_CANCELABLE", "domain": "a2a-protocol.org",
                    "metadata": {"taskId": "tsk_1", "state": state}}]}})
        };
        assert_eq!(
            report(ended("TASK_STATE_COMPLETED")).unwrap(),
            [
                "task:    tsk_1",
                "state:   completed",
                "nothing to cancel: the task had already ended",
            ]
        );
        assert_eq!(
            report(ended("ended-somehow")).unwrap()[1],
            "state:   ended-somehow"
        );
    }
}
