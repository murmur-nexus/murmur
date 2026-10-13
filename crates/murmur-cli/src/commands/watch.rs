use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpStream;

use serde_json::Value;

use crate::error::{CliError, E_IO_003};
use crate::live_address::Target;

/// Connect to a capsule's SSE observer endpoint (`murmur.WatchSession`) and print events to
/// stdout.
///
/// Unlike `SendStreamingMessage`, this does not submit a task. It passively observes the capsule's
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
        "method": "murmur.WatchSession",
        "params": {}
    })
    .to_string();

    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\n{}{}: {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nLast-Event-ID: 0\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
        super::cancel::authorization_line(target.door_token().as_ref()),
        capsule_runtime::A2A_VERSION_HEADER,
        capsule_runtime::A2A_PROTOCOL_VERSION,
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

    if let Some(refused) = super::cancel::door_refusal(&status_line) {
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
    capsule_runtime::report_eprintln!(
        "[murmur] watching {} — Ctrl-C ends the watch, not the capsule; use `mur stop` to end \
         the capsule",
        target.label()
    );

    match read_stream(reader, &mut std::io::stdout().lock()) {
        Ok(StreamEnd::CapsuleClosed) => {
            capsule_runtime::report_eprintln!("[murmur] capsule closed");
            Ok(())
        }
        // The reader has what it wanted; the capsule is left running.
        Ok(StreamEnd::ReaderClosed) => Ok(()),
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

/// How a `murmur.WatchSession` connection ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// The capsule wrote `capsule-closed`.
    CapsuleClosed,
    /// The stream reached EOF or a read error with no `capsule-closed`. `last_event_id` is the
    /// `id:` of the last complete frame that carried one, or `None` when no such frame arrived.
    /// Ids are unique and ascending within a capsule session, so it is the `Last-Event-ID` a new
    /// `murmur.WatchSession` connection resumes after.
    ConnectionLost { last_event_id: Option<u64> },
    /// Standard output was closed by its reader.
    ReaderClosed,
}

/// The stderr warning for a `lagged` frame, naming how many live frames this connection lost.
fn lagged_warning(missed: u64) -> String {
    format!("[murmur] warning: this connection missed {missed} events")
}

/// Read a `murmur.WatchSession` SSE body to its end, rendering task statuses, answers, tool
/// results and text to `out` and warnings to stderr.
///
/// Every event's `data:` is a JSON-RPC response whose `result` is an A2A v1.0 `StreamResponse`.
/// One carrying a murmur frame under `metadata[STREAM_EXTENSION_URI]` is read by its `frame`;
/// any other `statusUpdate` is a status line, and a `response` `artifactUpdate` is the answer the
/// task's `completed` line prints. Anything else is skipped.
///
/// `reader` is positioned after the HTTP response headers. A write to `out` that fails with
/// `BrokenPipe` ends the stream as [`StreamEnd::ReaderClosed`]; any other failed write is the
/// error. A failed read ends the stream as [`StreamEnd::ConnectionLost`].
pub(crate) fn read_stream(
    mut reader: impl BufRead,
    out: &mut impl Write,
) -> std::io::Result<StreamEnd> {
    let mut view = WatchView::default();

    let mut current_data = String::new();
    // An `id:` belongs to the event it appears in, so it is recorded only once that event's
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
            if !current_data.is_empty() {
                if let Ok(event) = serde_json::from_str::<Value>(&current_data) {
                    match view.dispatch(out, &event["result"]) {
                        Ok(Some(end)) => return Ok(end),
                        Ok(None) => {}
                        Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                            return Ok(StreamEnd::ReaderClosed)
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            current_data.clear();
            current_id = None;
        } else if line.starts_with(':') {
            // A comment line — the heartbeat — is not part of any event.
        } else if let Some(rest) = line.strip_prefix("data: ") {
            current_data = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("id: ") {
            current_id = rest.trim().parse().ok().or(current_id);
        }
    }
}

/// What `mur watch` remembers between events.
struct WatchView {
    conversation_mode: String,
    /// The turn each context is on, in threaded mode.
    context_turns: HashMap<String, u32>,
    /// Each task's answer, held from its `response` artifact until its `completed` line prints it.
    responses: HashMap<String, String>,
}

impl Default for WatchView {
    fn default() -> Self {
        Self {
            conversation_mode: String::from("stateless"),
            context_turns: HashMap::new(),
            responses: HashMap::new(),
        }
    }
}

/// The states a task ends in, in murmur's words.
const ENDED_STATES: [&str; 4] = ["completed", "failed", "canceled", "rejected"];

impl WatchView {
    /// Render one event's `StreamResponse`. `Some` when the event ends the stream.
    fn dispatch(
        &mut self,
        out: &mut impl Write,
        result: &Value,
    ) -> std::io::Result<Option<StreamEnd>> {
        let body = result
            .get("statusUpdate")
            .or_else(|| result.get("message"))
            .unwrap_or(&Value::Null);
        let frame = &body["metadata"][capsule_runtime::STREAM_EXTENSION_URI];
        if let Some(name) = frame.get("frame").and_then(Value::as_str) {
            return self.murmur_frame(out, name, frame);
        }
        if let Some(status) = result.get("statusUpdate") {
            self.status_line(out, status)?;
        } else if let Some(update) = result.get("artifactUpdate") {
            let artifact = &update["artifact"];
            if artifact["artifactId"] == "response" {
                let text = artifact_text(artifact);
                if let Some(task_id) = update["taskId"].as_str() {
                    self.responses.insert(task_id.to_string(), text);
                }
            }
        }
        // A `task` event, and any member this client does not know, is skipped, so the capsule
        // can add one without breaking the watch.
        Ok(None)
    }

    /// Render one murmur frame. Reasoning, tool calls being written and frames this client does
    /// not know print nothing.
    fn murmur_frame(
        &mut self,
        out: &mut impl Write,
        name: &str,
        frame: &Value,
    ) -> std::io::Result<Option<StreamEnd>> {
        match name {
            "connection-ack" => {
                if let Some(mode) = frame.get("conversationMode").and_then(Value::as_str) {
                    self.conversation_mode = mode.to_string();
                }
            }
            "gap" => {
                let first_id = frame
                    .get("firstAvailableId")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                capsule_runtime::report_eprintln!("[murmur] warning: buffer overflow — some earlier events were lost (first available id: {first_id})");
            }
            "lagged" => {
                let missed = frame.get("missed").and_then(Value::as_u64).unwrap_or(0);
                capsule_runtime::report_eprintln!("{}", lagged_warning(missed));
            }
            "capsule-closed" => return Ok(Some(StreamEnd::CapsuleClosed)),
            "text" => {
                if let Some(text) = frame.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        write!(out, "{text}")?;
                        out.flush()?;
                    }
                }
            }
            "artifact" => {
                let header = artifact_header(frame);
                let content = frame.get("content").and_then(Value::as_str).unwrap_or("");
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
            _ => {}
        }
        Ok(None)
    }

    /// Render one task status: `[<state>]  <message>`, or for a task that ended `[<state>]` alone,
    /// followed by the wrapped answer of a `completed` one. Threaded mode prefixes the context
    /// and its turn, which a task's end advances.
    fn status_line(&mut self, out: &mut impl Write, update: &Value) -> std::io::Result<()> {
        let task_id = update["taskId"].as_str().unwrap_or("").to_string();
        let status = &update["status"];
        let state = status["state"]
            .as_str()
            .map(|wire| capsule_runtime::murmur_state_name(wire).unwrap_or(wire))
            .unwrap_or("unknown");
        let message = status["message"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default();
        let ended = ENDED_STATES.contains(&state);

        let prefix = if self.conversation_mode == "threaded" {
            let cid = update["contextId"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| task_id.clone());
            let turn = *self.context_turns.entry(cid.clone()).or_insert(1);
            if ended {
                *self.context_turns.entry(cid.clone()).or_insert(1) += 1;
            }
            format!("[{} / turn {turn} / {state}]", truncate_context_id(&cid))
        } else {
            format!("[{state}]")
        };

        if !ended {
            return writeln!(out, "{prefix}  {message}");
        }
        writeln!(out, "{prefix}")?;
        let response = self.responses.remove(&task_id);
        if state == "completed" {
            let col_width = terminal_width().saturating_sub(2);
            for line in wrap_text(&response.unwrap_or_default(), col_width) {
                writeln!(out, "  {line}")?;
            }
            writeln!(out)?;
        }
        Ok(())
    }
}

/// The text of an artifact's text parts, joined.
fn artifact_text(artifact: &Value) -> String {
    artifact["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect()
        })
        .unwrap_or_default()
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

/// The `[artifact]` line's header for one `artifact` frame, without the content that follows it:
/// `[artifact] tool: bash [error, exit 2, 1204ms]`.
///
/// The bracketed outcome lists `ok` or `error`, then `exit <n>`, `<n>ms` and `truncated` for
/// whichever of those the frame reports. A frame with no `isError` key comes from a runtime that
/// reports no outcome, and gets no brackets rather than an `ok` it never claimed.
fn artifact_header(artifact: &Value) -> String {
    let tool_name = artifact
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let Some(is_error) = artifact.get("isError").and_then(Value::as_bool) else {
        return format!("[artifact] tool: {tool_name}");
    };
    let mut outcome = vec![if is_error { "error" } else { "ok" }.to_string()];
    if let Some(exit_code) = artifact.get("exitCode").and_then(Value::as_i64) {
        outcome.push(format!("exit {exit_code}"));
    }
    if let Some(duration_ms) = artifact.get("durationMs").and_then(Value::as_u64) {
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
    use capsule_runtime::STREAM_EXTENSION_URI;

    /// One SSE event: an `id:` line when `id` is given, and a `data:` line holding a JSON-RPC
    /// response whose `result` is `result`.
    fn event(id: Option<u64>, result: serde_json::Value) -> String {
        let id_line = id.map(|id| format!("id: {id}\n")).unwrap_or_default();
        let data = json!({"jsonrpc": "2.0", "id": 1, "result": result});
        format!("{id_line}data: {data}\n\n")
    }

    /// A murmur frame that belongs to no task, carried by a `message`.
    fn message_frame(frame: serde_json::Value) -> String {
        let name = frame["frame"].clone();
        event(
            None,
            json!({"message": {
                "messageId": "msg_1",
                "role": "ROLE_AGENT",
                "parts": [{"text": name, "mediaType": "text/plain"}],
                "metadata": {STREAM_EXTENSION_URI: frame},
            }}),
        )
    }

    /// A murmur frame of `tsk_1`, carried by a `statusUpdate`.
    fn task_frame(id: u64, frame: serde_json::Value) -> String {
        event(
            Some(id),
            json!({"statusUpdate": {
                "taskId": "tsk_1",
                "contextId": "ctx_1",
                "status": {"state": "TASK_STATE_WORKING"},
                "metadata": {STREAM_EXTENSION_URI: frame},
            }}),
        )
    }

    /// A status of `tsk_1` in `state`, saying `message` when given.
    fn status(id: Option<u64>, state: &str, message: Option<&str>) -> String {
        let mut status = json!({"state": state});
        if let Some(text) = message {
            status["message"] = json!({
                "messageId": "msg_2", "contextId": "ctx_1", "taskId": "tsk_1",
                "role": "ROLE_AGENT", "parts": [{"text": text, "mediaType": "text/plain"}],
            });
        }
        event(
            id,
            json!({"statusUpdate": {"taskId": "tsk_1", "contextId": "ctx_1", "status": status}}),
        )
    }

    /// The `response` artifact of `tsk_1`, holding `text`.
    fn response(id: u64, text: &str) -> String {
        event(
            Some(id),
            json!({"artifactUpdate": {
                "taskId": "tsk_1",
                "contextId": "ctx_1",
                "artifact": {
                    "artifactId": "response",
                    "name": "response",
                    "parts": [{"text": text, "mediaType": "text/plain"}],
                },
                "lastChunk": true,
            }}),
        )
    }

    /// `tsk_1` completing with `answer`: its response, then its terminal status.
    fn completed(id: u64, answer: &str) -> String {
        format!(
            "{}{}",
            response(id, answer),
            status(Some(id + 1), "TASK_STATE_COMPLETED", Some("session ended"))
        )
    }

    fn closed() -> String {
        message_frame(json!({"frame": "capsule-closed"}))
    }

    /// Read `body` as a `murmur.WatchSession` SSE body, returning how it ended and what was
    /// rendered.
    fn read(body: &str) -> (StreamEnd, String) {
        let mut out = Vec::new();
        let end = read_stream(body.as_bytes(), &mut out).unwrap();
        (end, String::from_utf8(out).unwrap())
    }

    fn ack() -> String {
        message_frame(
            json!({"frame": "connection-ack", "role": "observer", "conversationMode": "stateless"}),
        )
    }

    #[test]
    fn capsule_closed_ends_the_stream_as_closed() {
        let body = format!("{}{}", ack(), closed());
        assert_eq!(read(&body).0, StreamEnd::CapsuleClosed);
    }

    #[test]
    fn eof_after_frames_with_ids_reports_the_last_id() {
        let body = format!(
            "{}{}{}",
            ack(),
            status(Some(0), "TASK_STATE_WORKING", Some("inference turn 1")),
            task_frame(7, json!({"frame": "text", "text": "hi", "final": false}))
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
            read(&ack()).0,
            StreamEnd::ConnectionLost {
                last_event_id: None
            }
        );
    }

    #[test]
    fn frames_without_an_id_keep_the_last_recorded_id() {
        let body = format!(
            "{}{}{}",
            task_frame(4, json!({"frame": "text", "text": "", "final": true})),
            message_frame(json!({"frame": "gap", "firstAvailableId": 2})),
            ack()
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
        let body = format!(
            "{}id: 5\n",
            task_frame(4, json!({"frame": "text", "text": "", "final": true}))
        );
        assert_eq!(
            read(&body).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(4)
            }
        );
    }

    #[test]
    fn heartbeat_changes_nothing() {
        let frame = status(Some(3), "TASK_STATE_WORKING", Some("inference turn 1"));
        let with = format!("{}:heartbeat\n\n{frame}:heartbeat\n\n", ack());
        let without = format!("{}{frame}", ack());
        assert_eq!(read(&with), read(&without));
        assert_eq!(
            read(&with).0,
            StreamEnd::ConnectionLost {
                last_event_id: Some(3)
            }
        );
    }

    fn lagged() -> String {
        message_frame(json!({"frame": "lagged", "missed": 3}))
    }

    #[test]
    fn a_lagged_frame_renders_nothing_and_the_stream_continues() {
        let first = status(Some(1), "TASK_STATE_WORKING", Some("inference turn 1"));
        let second = completed(5, "done");
        let (ack, lagged, closed) = (ack(), lagged(), closed());
        let (end, rendered) = read(&format!("{ack}{first}{lagged}{second}{closed}"));
        assert_eq!(end, StreamEnd::CapsuleClosed);
        let (_, without_lag) = read(&format!("{ack}{first}{second}{closed}"));
        assert!(rendered.contains("[completed]"), "rendered: {rendered}");
        assert_eq!(rendered, without_lag);
    }

    #[test]
    fn eof_after_a_lagged_frame_reports_the_preceding_id() {
        let body = format!(
            "{}{}{}",
            ack(),
            task_frame(9, json!({"frame": "text", "text": "hi", "final": false})),
            lagged()
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
            "{}{}{}{}{}{}",
            ack(),
            task_frame(
                1,
                json!({"frame": "thinking", "text": "hmm", "final": false})
            ),
            task_frame(2, json!({"frame": "some-future-frame", "anything": true})),
            event(Some(3), json!({"someFutureMember": {"anything": true}})),
            event(
                None,
                json!({"task": {"id": "tsk_1", "contextId": "ctx_1", "status": {"state": "TASK_STATE_SUBMITTED"}}})
            ),
            closed()
        );
        let (end, rendered) = read(&body);
        assert_eq!(end, StreamEnd::CapsuleClosed);
        assert_eq!(rendered, "");
    }

    #[test]
    fn unknown_keys_render_as_if_absent() {
        let artifact = json!({
            "frame": "artifact", "toolName": "bash", "content": "a\nb", "fenceSource": null,
            "toolCallId": "c1", "isError": false, "durationMs": 4, "exitCode": 0,
            "truncated": false,
        });
        let plain = format!(
            "{}{}",
            completed(1, "done"),
            task_frame(3, artifact.clone())
        );
        let mut later = artifact;
        later["summary"] = json!("x");
        let extended = format!(
            "{}{}{}",
            response(1, "done").replace("\"lastChunk\"", "\"extra\":1,\"lastChunk\""),
            status(Some(2), "TASK_STATE_COMPLETED", Some("session ended"))
                .replace("\"status\":{", "\"later\":{},\"status\":{\"newKey\":[1],"),
            task_frame(3, later)
        );
        let (plain, extended) = (plain.as_str(), extended.as_str());
        let (_, plain_out) = read(plain);
        assert_eq!(
            plain_out,
            "[completed]\n  done\n\n[artifact] tool: bash [ok, exit 0, 4ms] | a\n  b\n"
        );
        assert_eq!(read(extended).1, plain_out);
    }

    #[test]
    fn statuses_print_in_murmurs_words_and_threaded_mode_numbers_turns() {
        let body = format!(
            "{}{}{}{}",
            message_frame(
                json!({"frame": "connection-ack", "role": "observer", "conversationMode": "threaded"})
            ),
            status(Some(1), "TASK_STATE_INPUT_REQUIRED", Some("Which file?")),
            status(Some(2), "TASK_STATE_FAILED", Some("boom")),
            status(Some(3), "TASK_STATE_WORKING", Some("inference turn 1")),
        );
        assert_eq!(
            read(&body).1,
            "[ctx_1... / turn 1 / input-required]  Which file?\n\
             [ctx_1... / turn 1 / failed]\n\
             [ctx_1... / turn 2 / working]  inference turn 1\n"
        );
    }

    #[test]
    fn successful_shell_call_reports_ok_exit_and_duration() {
        let artifact = json!({
            "toolName": "bash", "content": "$ echo hello", "fenceSource": "tool:bash",
            "toolCallId": "call_1", "isError": false, "durationMs": 12, "exitCode": 0,
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
            "toolName": "bash", "content": "", "fenceSource": "tool:bash",
            "toolCallId": "call_1", "isError": true, "durationMs": 1204, "exitCode": 2,
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
            "toolName": "write-file", "content": "permission denied", "fenceSource": null,
            "toolCallId": "call_1", "isError": true, "durationMs": 3, "exitCode": null,
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
            "toolName": "read-file", "content": "partial", "fenceSource": "tool:read-file",
            "toolCallId": "call_1", "isError": false, "durationMs": 5, "exitCode": null,
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
            "toolName": "my-hook", "content": "{\"reviewed\":true}", "fenceSource": null,
            "toolCallId": null, "isError": false, "durationMs": null, "exitCode": null,
            "truncated": false,
        });
        assert_eq!(artifact_header(&artifact), "[artifact] tool: my-hook [ok]");
    }

    #[test]
    fn frame_from_a_runtime_without_outcome_fields_renders_without_brackets() {
        let artifact = json!({ "toolName": "bash", "content": "hello" });
        assert_eq!(artifact_header(&artifact), "[artifact] tool: bash");
    }

    /// A stdout whose every write fails with `kind`.
    struct Refusing(std::io::ErrorKind);

    impl std::io::Write for Refusing {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0, "refused"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(self.0, "refused"))
        }
    }

    fn status_then_more() -> String {
        format!(
            "{}{}",
            status(Some(0), "TASK_STATE_WORKING", Some("inference turn 1")),
            closed()
        )
    }

    #[test]
    fn a_closed_reader_ends_the_stream_as_reader_closed() {
        let body = format!("{}{}", ack(), status_then_more());
        let end = read_stream(
            body.as_bytes(),
            &mut Refusing(std::io::ErrorKind::BrokenPipe),
        )
        .unwrap();
        assert_eq!(end, StreamEnd::ReaderClosed);
    }

    #[test]
    fn any_other_write_failure_is_an_error() {
        let body = format!("{}{}", ack(), status_then_more());
        let error = read_stream(
            body.as_bytes(),
            &mut Refusing(std::io::ErrorKind::StorageFull),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
    }
}
