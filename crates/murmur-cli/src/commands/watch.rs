use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};
use crate::live_address::Target;

/// Connect to a capsule's SSE observer endpoint (`stream/watch`) and print events to stdout.
///
/// Unlike `message/stream`, this does not submit a task. It passively observes the capsule's
/// SSE stream, including any events buffered since the capsule started. The process stays
/// connected across task turns. It returns `Ok` only on `capsule-closed`; a stream that ends any
/// other way is `E_IO_003`, because the capsule behind it may still be running.
///
/// The capsule is already resolved: a session address was verified against the running record
/// before this ran, so an unreachable capsule is reported as such rather than as a refused
/// connection.
pub(crate) fn run_watch(target: &Target) -> Result<(), CliError> {
    let addr = target
        .url()
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

    // Which tool the operator is looking at, said once, on stderr so piping stdout is unaffected.
    // The same keystroke means opposite things in the two places a capsule is watched from: here
    // it ends the watch, in the nexus CLI it ends the capsule.
    eprintln!(
        "[murmur] watching {} — Ctrl-C ends the watch, not the capsule; use `mur stop` to end \
         the capsule",
        target.label()
    );

    match read_stream(reader, &mut std::io::stdout().lock()) {
        Ok(StreamEnd::CapsuleClosed) => {
            eprintln!("[murmur] capsule closed");
            Ok(())
        }
        Ok(StreamEnd::ConnectionLost { last_event_id }) => {
            let position = match last_event_id {
                Some(id) => format!("after event id {id}"),
                None => "before any event".to_string(),
            };
            Err(CliError::new(
                E_IO_003,
                format!(
                    "connection to {} lost {position} — the capsule may still be running; run \
                     mur watch again to reattach",
                    target.label()
                ),
            ))
        }
        Err(e) => Err(CliError::new(
            E_IO_003,
            format!("failed to write to stdout: {e}"),
        )),
    }
}

/// How a `stream/watch` connection ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// The capsule wrote `capsule-closed`.
    CapsuleClosed,
    /// The stream reached EOF or a read error with no `capsule-closed`. `last_event_id` is the
    /// `id:` of the last complete frame that carried one, or `None` when no such frame arrived.
    /// Ids are unique and ascending within a capsule session, so it is the `Last-Event-ID` a new
    /// `stream/watch` connection resumes after.
    ConnectionLost { last_event_id: Option<u64> },
}

/// The stderr warning for a `lagged` frame, naming how many live frames this connection lost.
fn lagged_warning(missed: u64) -> String {
    format!("[murmur] warning: this connection missed {missed} events")
}

/// Read a `stream/watch` SSE body to its end, rendering `status`, `artifact` and `text` frames to
/// `out` and warnings to stderr.
///
/// `reader` is positioned after the HTTP response headers. Errors only when writing to `out`
/// fails; a failed read ends the stream as [`StreamEnd::ConnectionLost`].
pub(crate) fn read_stream(
    mut reader: impl BufRead,
    out: &mut impl Write,
) -> std::io::Result<StreamEnd> {
    let mut conversation_mode = String::from("stateless");
    let mut task_context_map: HashMap<String, String> = HashMap::new();
    let mut context_turns: HashMap<String, u32> = HashMap::new();

    let mut current_event_type = String::new();
    let mut current_data = String::new();
    // An `id:` belongs to the frame it appears in, so it is recorded only once that frame's
    // terminating blank line arrives.
    let mut current_id: Option<u64> = None;
    let mut last_event_id: Option<u64> = None;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Ok(StreamEnd::ConnectionLost { last_event_id }),
            Ok(_) => {}
        }

        let line = line.trim_end_matches('\n').trim_end_matches('\r');

        if line.is_empty() {
            if current_id.is_some() {
                last_event_id = current_id;
            }
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
                    "lagged" => {
                        let missed = serde_json::from_str::<Value>(&current_data)
                            .ok()
                            .and_then(|v| v.get("missed").and_then(Value::as_u64))
                            .unwrap_or(0);
                        eprintln!("{}", lagged_warning(missed));
                    }
                    "capsule-closed" => return Ok(StreamEnd::CapsuleClosed),
                    "status" | "artifact" | "text" => {
                        // A `final` status ends one task, not the capsule, so the watch goes on.
                        dispatch_sse_event(
                            out,
                            &current_event_type,
                            &current_data,
                            &conversation_mode,
                            &mut task_context_map,
                            &mut context_turns,
                        )?;
                    }
                    // Reasoning chunks are not shown.
                    "thinking" => {}
                    // Forward compatibility: an event type this client does not know is skipped,
                    // so the capsule can add one without breaking the watch.
                    _ => {}
                }
            }
            current_event_type.clear();
            current_data.clear();
            current_id = None;
        } else if line.starts_with(':') {
            // A comment line — the heartbeat — is not part of any frame.
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event_type = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            current_data = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("id: ") {
            current_id = rest.trim().parse().ok().or(current_id);
        }
    }
}

/// Render one `status`, `artifact` or `text` frame to `out`. Keys a frame carries beyond the ones
/// read here are ignored.
fn dispatch_sse_event(
    out: &mut impl Write,
    event_type: &str,
    data: &str,
    conversation_mode: &str,
    task_context_map: &mut HashMap<String, String>,
    context_turns: &mut HashMap<String, u32>,
) -> std::io::Result<()> {
    let is_final = data.contains("\"final\":true");
    let is_threaded = conversation_mode == "threaded";

    match event_type {
        "status" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                let task_id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let context_id_opt = event
                    .get("context_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
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
                    let prefix = format!("[{} / turn {turn} / {state}]", truncate_context_id(&cid));

                    if is_final {
                        writeln!(out, "{prefix}")?;
                        if state == "completed" {
                            if let Some(ref resp) = response {
                                let col_width = terminal_width().saturating_sub(2);
                                for line in wrap_text(resp, col_width) {
                                    writeln!(out, "  {line}")?;
                                }
                                writeln!(out)?;
                            }
                        }
                        *context_turns.entry(cid).or_insert(1) += 1;
                    } else {
                        writeln!(out, "{prefix}  {message}")?;
                    }
                } else {
                    if is_final {
                        writeln!(out, "[{state}]")?;
                        if state == "completed" {
                            if let Some(ref resp) = response {
                                let col_width = terminal_width().saturating_sub(2);
                                for line in wrap_text(resp, col_width) {
                                    writeln!(out, "  {line}")?;
                                }
                                writeln!(out)?;
                            }
                        }
                    } else {
                        writeln!(out, "[{state}]  {message}")?;
                    }
                }
            }
        }
        "artifact" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                let artifact = event.get("artifact").unwrap_or(&Value::Null);
                let header = artifact_header(artifact);
                let content = artifact
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut content_lines = content.lines();
                if let Some(first) = content_lines.next() {
                    writeln!(out, "{header} | {first}")?;
                    for rest in content_lines {
                        writeln!(out, "  {rest}")?;
                    }
                } else {
                    writeln!(out, "{header}")?;
                }
            }
        }
        "text" => {
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        write!(out, "{text}")?;
                        out.flush()?;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
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

/// The `[artifact]` line's header for one frame's `artifact` object, without the content that
/// follows it: `[artifact] tool: bash [error, exit 2, 1204ms]`.
///
/// The bracketed outcome lists `ok` or `error`, then `exit <n>`, `<n>ms` and `truncated` for
/// whichever of those the frame reports. A frame with no `is_error` key comes from a runtime that
/// reports no outcome, and gets no brackets rather than an `ok` it never claimed.
fn artifact_header(artifact: &Value) -> String {
    let tool_name = artifact
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let Some(is_error) = artifact.get("is_error").and_then(Value::as_bool) else {
        return format!("[artifact] tool: {tool_name}");
    };
    let mut outcome = vec![if is_error { "error" } else { "ok" }.to_string()];
    if let Some(exit_code) = artifact.get("exit_code").and_then(Value::as_i64) {
        outcome.push(format!("exit {exit_code}"));
    }
    if let Some(duration_ms) = artifact.get("duration_ms").and_then(Value::as_u64) {
        outcome.push(format!("{duration_ms}ms"));
    }
    if artifact.get("truncated").and_then(Value::as_bool) == Some(true) {
        outcome.push("truncated".to_string());
    }
    format!("[artifact] tool: {tool_name} [{}]", outcome.join(", "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{artifact_header, lagged_warning, read_stream, StreamEnd};

    /// Read `body` as a `stream/watch` SSE body, returning how it ended and what was rendered.
    fn read(body: &str) -> (StreamEnd, String) {
        let mut out = Vec::new();
        let end = read_stream(body.as_bytes(), &mut out).unwrap();
        (end, String::from_utf8(out).unwrap())
    }

    const ACK: &str =
        "event: connection-ack\ndata: {\"role\":\"observer\",\"conversation_mode\":\"stateless\"}\n\n";

    #[test]
    fn capsule_closed_ends_the_stream_as_closed() {
        let body = format!("{ACK}event: capsule-closed\ndata: {{}}\n\n");
        assert_eq!(read(&body).0, StreamEnd::CapsuleClosed);
    }

    #[test]
    fn eof_after_frames_with_ids_reports_the_last_id() {
        let body = format!(
            "{ACK}id: 0\nevent: status\ndata: {{\"id\":\"tsk_1\",\"status\":{{\"state\":\"working\",\"message\":\"inference turn 1\"}},\"final\":false}}\n\n\
             id: 7\nevent: text\ndata: {{\"id\":\"tsk_1\",\"text\":\"hi\",\"final\":false}}\n\n"
        );
        assert_eq!(
            read(&body).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(7)
            }
        );
    }

    #[test]
    fn eof_after_only_the_ack_reports_no_id() {
        assert_eq!(
            read(ACK).0,
            StreamEnd::ConnectionLost {
                last_event_id: None
            }
        );
    }

    #[test]
    fn frames_without_an_id_keep_the_last_recorded_id() {
        let body = format!(
            "id: 4\nevent: text\ndata: {{\"id\":\"tsk_1\",\"text\":\"\",\"final\":true}}\n\n\
             event: gap\ndata: {{\"first_available_id\":2}}\n\n{ACK}"
        );
        assert_eq!(
            read(&body).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(4)
            }
        );
    }

    #[test]
    fn an_id_is_recorded_only_once_its_frame_is_complete() {
        let body = "id: 4\nevent: text\ndata: {\"id\":\"tsk_1\",\"text\":\"\",\"final\":true}\n\n\
                    id: 5\nevent: text\n";
        assert_eq!(
            read(body).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(4)
            }
        );
    }

    #[test]
    fn heartbeat_changes_nothing() {
        let frame = "id: 3\nevent: status\ndata: {\"id\":\"tsk_1\",\"status\":{\"state\":\"working\",\"message\":\"inference turn 1\"},\"final\":false}\n\n";
        let with = format!("{ACK}:heartbeat\n\n{frame}:heartbeat\n\n");
        let without = format!("{ACK}{frame}");
        assert_eq!(read(&with), read(&without));
        assert_eq!(
            read(&with).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(3)
            }
        );
    }

    const LAGGED: &str = "event: lagged\ndata: {\"missed\":3}\n\n";

    #[test]
    fn a_lagged_frame_renders_nothing_and_the_stream_continues() {
        let first = "id: 1\nevent: status\ndata: {\"id\":\"tsk_1\",\"status\":{\"state\":\"working\",\"message\":\"inference turn 1\"},\"final\":false}\n\n";
        let second = "id: 5\nevent: status\ndata: {\"id\":\"tsk_1\",\"status\":{\"state\":\"completed\",\"message\":\"session ended\",\"response\":\"done\"},\"final\":true}\n\n";
        let closed = "event: capsule-closed\ndata: {}\n\n";
        let (end, rendered) = read(&format!("{ACK}{first}{LAGGED}{second}{closed}"));
        assert_eq!(end, StreamEnd::CapsuleClosed);
        let (_, without_lag) = read(&format!("{ACK}{first}{second}{closed}"));
        assert!(rendered.contains("[completed]"), "rendered: {rendered}");
        assert_eq!(rendered, without_lag);
    }

    #[test]
    fn eof_after_a_lagged_frame_reports_the_preceding_id() {
        let body = format!(
            "{ACK}id: 9\nevent: text\ndata: {{\"id\":\"tsk_1\",\"text\":\"hi\",\"final\":false}}\n\n{LAGGED}"
        );
        assert_eq!(
            read(&body).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(9)
            }
        );
    }

    #[test]
    fn lagged_frame_warning_names_the_count() {
        assert_eq!(
            lagged_warning(3),
            "[murmur] warning: this connection missed 3 events"
        );
    }

    #[test]
    fn thinking_and_unknown_event_types_are_consumed_without_ending_the_stream() {
        let body = format!(
            "{ACK}id: 1\nevent: thinking\ndata: {{\"id\":\"tsk_1\",\"text\":\"hmm\",\"final\":false}}\n\n\
             id: 2\nevent: some-future-type\ndata: {{\"anything\":true}}\n\n\
             event: capsule-closed\ndata: {{}}\n\n"
        );
        let (end, rendered) = read(&body);
        assert_eq!(end, StreamEnd::CapsuleClosed);
        assert_eq!(rendered, "");
    }

    #[test]
    fn unknown_keys_render_as_if_absent() {
        let plain = "event: status\ndata: {\"id\":\"tsk_1\",\"context_id\":\"ctx_1\",\"status\":{\"state\":\"completed\",\"message\":\"session ended\",\"response\":\"done\"},\"final\":true}\n\n\
                     event: artifact\ndata: {\"id\":\"tsk_1\",\"artifact\":{\"tool_name\":\"bash\",\"content\":\"a\\nb\",\"fence_source\":null,\"tool_call_id\":\"c1\",\"is_error\":false,\"duration_ms\":4,\"exit_code\":0,\"truncated\":false}}\n\n";
        let extended = "event: status\ndata: {\"id\":\"tsk_1\",\"context_id\":\"ctx_1\",\"extra\":1,\"status\":{\"state\":\"completed\",\"message\":\"session ended\",\"response\":\"done\",\"new_key\":[1]},\"final\":true}\n\n\
                        event: artifact\ndata: {\"id\":\"tsk_1\",\"later\":{},\"artifact\":{\"tool_name\":\"bash\",\"content\":\"a\\nb\",\"fence_source\":null,\"tool_call_id\":\"c1\",\"is_error\":false,\"duration_ms\":4,\"exit_code\":0,\"truncated\":false,\"summary\":\"x\"}}\n\n";
        let (_, plain_out) = read(plain);
        assert_eq!(
            plain_out,
            "[completed]\n  done\n\n[artifact] tool: bash [ok, exit 0, 4ms] | a\n  b\n"
        );
        assert_eq!(read(extended).1, plain_out);
    }

    #[test]
    fn successful_shell_call_reports_ok_exit_and_duration() {
        let artifact = json!({
            "tool_name": "bash", "content": "$ echo hello", "fence_source": "tool:bash",
            "tool_call_id": "call_1", "is_error": false, "duration_ms": 12, "exit_code": 0,
            "truncated": false,
        });
        assert_eq!(
            artifact_header(&artifact),
            "[artifact] tool: bash [ok, exit 0, 12ms]"
        );
    }

    #[test]
    fn failed_shell_call_reports_error_and_its_exit_code() {
        let artifact = json!({
            "tool_name": "bash", "content": "", "fence_source": "tool:bash",
            "tool_call_id": "call_1", "is_error": true, "duration_ms": 1204, "exit_code": 2,
            "truncated": false,
        });
        assert_eq!(
            artifact_header(&artifact),
            "[artifact] tool: bash [error, exit 2, 1204ms]"
        );
    }

    #[test]
    fn failed_call_without_a_subprocess_has_no_exit_segment() {
        let artifact = json!({
            "tool_name": "write-file", "content": "permission denied", "fence_source": null,
            "tool_call_id": "call_1", "is_error": true, "duration_ms": 3, "exit_code": null,
            "truncated": false,
        });
        assert_eq!(
            artifact_header(&artifact),
            "[artifact] tool: write-file [error, 3ms]"
        );
    }

    #[test]
    fn truncated_call_ends_with_truncated() {
        let artifact = json!({
            "tool_name": "read-file", "content": "partial", "fence_source": "tool:read-file",
            "tool_call_id": "call_1", "is_error": false, "duration_ms": 5, "exit_code": null,
            "truncated": true,
        });
        assert_eq!(
            artifact_header(&artifact),
            "[artifact] tool: read-file [ok, 5ms, truncated]"
        );
    }

    #[test]
    fn hook_artifact_reports_only_ok() {
        let artifact = json!({
            "tool_name": "my-hook", "content": "{\"reviewed\":true}", "fence_source": null,
            "tool_call_id": null, "is_error": false, "duration_ms": null, "exit_code": null,
            "truncated": false,
        });
        assert_eq!(artifact_header(&artifact), "[artifact] tool: my-hook [ok]");
    }

    #[test]
    fn frame_from_a_runtime_without_outcome_fields_renders_without_brackets() {
        let artifact = json!({ "tool_name": "bash", "content": "hello" });
        assert_eq!(artifact_header(&artifact), "[artifact] tool: bash");
    }
}
