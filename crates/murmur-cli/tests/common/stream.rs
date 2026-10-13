//! Reading the A2A v1.0 streams a door writes: `SendStreamingMessage` and `stream/watch`.
//!
//! Every event is an optional `id:` line and one `data:` line holding a JSON-RPC response whose
//! `result` is an `lf.a2a.v1.StreamResponse`. A murmur frame rides in a `statusUpdate` or a
//! `message` under `metadata[STREAM_EXTENSION_URI]`, and is read here by its `frame`.

use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use serde_json::{json, Value};

pub use capsule_runtime::STREAM_EXTENSION_URI;

use super::door_capsule::{Response, A2A_VERSION};

/// One event of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Its place in the session's sequence, or `None` for an event written to one connection only.
    pub id: Option<u64>,
    /// The whole JSON-RPC response its `data:` line carries.
    pub json: Value,
}

impl Event {
    /// The `StreamResponse`.
    pub fn result(&self) -> &Value {
        &self.json["result"]
    }

    /// The one member the `StreamResponse` sets, and its body.
    fn member(&self) -> (&str, &Value) {
        self.result()
            .as_object()
            .and_then(|object| object.iter().next())
            .map(|(name, body)| (name.as_str(), body))
            .unwrap_or_else(|| panic!("a StreamResponse sets one member: {}", self.json))
    }

    /// The murmur frame this event carries, or `null` for a standard event.
    pub fn frame(&self) -> &Value {
        let extension = &self.member().1["metadata"][STREAM_EXTENSION_URI];
        if extension.get("frame").is_some() {
            extension
        } else {
            &Value::Null
        }
    }

    /// What the event is: its murmur frame's name, or its member's (`task`, `message`,
    /// `status-update`, `artifact-update`).
    pub fn kind(&self) -> String {
        if let Some(frame) = self.frame()["frame"].as_str() {
            return frame.to_string();
        }
        match self.member().0 {
            "statusUpdate" => "status-update".to_string(),
            "artifactUpdate" => "artifact-update".to_string(),
            other => other.to_string(),
        }
    }

    /// Whether the event carries a murmur frame.
    pub fn is_frame(&self) -> bool {
        !self.frame().is_null()
    }

    /// The task the event belongs to: a `task`'s `id`, an update's `taskId`.
    pub fn task_id(&self) -> Option<&str> {
        match self.member() {
            ("task", task) => task["id"].as_str(),
            (_, body) => body["taskId"].as_str(),
        }
    }

    /// The `status.state` of a `task` or a `statusUpdate`, as the wire spells it.
    pub fn state(&self) -> Option<&str> {
        match self.member() {
            ("task" | "statusUpdate", body) => body["status"]["state"].as_str(),
            _ => None,
        }
    }

    /// [`Self::state`] in murmur's word.
    pub fn murmur_state(&self) -> Option<&'static str> {
        self.state().and_then(capsule_runtime::murmur_state_name)
    }

    /// The text of a `task`'s or a `statusUpdate`'s `status.message`.
    pub fn status_text(&self) -> Option<&str> {
        match self.member() {
            ("task" | "statusUpdate", body) => {
                body["status"]["message"]["parts"][0]["text"].as_str()
            }
            _ => None,
        }
    }

    /// The `status` of a `task` or a `statusUpdate`.
    pub fn status(&self) -> &Value {
        &self.member().1["status"]
    }

    /// The artifact an `artifactUpdate` delivers.
    pub fn artifact(&self) -> Option<&Value> {
        match self.member() {
            ("artifactUpdate", update) => Some(&update["artifact"]),
            _ => None,
        }
    }

    /// The text of the first part of the artifact an `artifactUpdate` delivers.
    pub fn artifact_text(&self) -> Option<&str> {
        self.artifact()?["parts"][0]["text"].as_str()
    }

    /// Whether this is a standard status in a state a task ends in.
    pub fn is_terminal_status(&self) -> bool {
        !self.is_frame()
            && self.member().0 == "statusUpdate"
            && matches!(
                self.murmur_state(),
                Some("completed" | "failed" | "canceled" | "rejected")
            )
    }

    /// The label the parity tests compare sequences with: a `working` status with its message,
    /// any other status by its state, a `text` frame as a chunk or final, anything else by kind.
    pub fn label(&self) -> String {
        match self.kind().as_str() {
            "status-update" => match self.murmur_state() {
                Some("working") => {
                    format!("status:working:{}", self.status_text().unwrap_or_default())
                }
                state => format!("status:{}", state.unwrap_or("unknown")),
            },
            "text" => match self.frame()["final"] == true {
                true => "text:final".to_string(),
                false => "text:chunk".to_string(),
            },
            other => other.to_string(),
        }
    }
}

/// Every event of an SSE body, heartbeats skipped, in order.
///
/// Panics on anything but the event form the door writes: an optional `id:` line, then one
/// `data:` line, then a blank line, with no `event:` line. A trailing event without its blank
/// line, cut off by a closed connection, is dropped.
pub fn parse(body: &str) -> Vec<Event> {
    let mut events = Vec::new();
    let mut id = None;
    let mut data: Option<String> = None;
    for line in body.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(data) = data.take() {
                let json: Value = serde_json::from_str(&data)
                    .unwrap_or_else(|e| panic!("data is not JSON ({e}): {data}"));
                assert_eq!(json["jsonrpc"], "2.0", "{json}");
                events.push(Event { id, json });
            }
            id = None;
        } else if line.starts_with(':') {
            assert_eq!(line, ":heartbeat", "the only comment is the heartbeat");
        } else if let Some(rest) = line.strip_prefix("id: ") {
            assert!(data.is_none(), "an id line follows a data line: {body}");
            id = Some(rest.parse().unwrap_or_else(|_| panic!("id {rest:?}")));
        } else if let Some(rest) = line.strip_prefix("data: ") {
            assert!(data.is_none(), "two data lines in one event: {body}");
            data = Some(rest.to_string());
        } else {
            panic!("a line that is not an id, data or heartbeat: {line:?}");
        }
    }
    events
}

/// The events of `events` that belong to `task_id`.
pub fn of_task<'a>(events: &'a [Event], task_id: &str) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| event.task_id() == Some(task_id))
        .collect()
}

/// The `A2A-Extensions` request header activating the stream extension.
pub fn extensions_header() -> (&'static str, &'static str) {
    (capsule_runtime::A2A_EXTENSIONS_HEADER, STREAM_EXTENSION_URI)
}

/// One `SendStreamingMessage` with JSON-RPC id `1` and `params`, presenting `token` as a bearer
/// token when given, activating the stream extension when `extensions` is set. Read to the end
/// of the stream, and checked against A2A v1.0 by the wire recorder.
pub fn stream_message(
    addr: &str,
    token: Option<&str>,
    params: Value,
    extensions: bool,
) -> (Response, Vec<Event>) {
    let extension = extensions.then(extensions_header);
    stream_message_with(addr, token, json!(1), params, extension.as_slice())
}

/// [`stream_message`] with request id `id` and `extra` request headers, sent as given.
pub fn stream_message_with(
    addr: &str,
    token: Option<&str>,
    id: Value,
    params: Value,
    extra: &[(&str, &str)],
) -> (Response, Vec<Event>) {
    let body =
        json!({"jsonrpc": "2.0", "id": id, "method": "SendStreamingMessage", "params": params})
            .to_string();
    let authorization = token.map(|token| format!("Bearer {token}"));
    let mut headers: Vec<(&str, &str)> = authorization
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .chain([A2A_VERSION])
        .collect();
    headers.extend_from_slice(extra);
    let response = read_to_end(addr, &headers, &body);
    let events = if response
        .header("content-type")
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        parse(&response.body)
    } else {
        Vec::new()
    };
    (response, events)
}

/// How long a stream may stay silent, heartbeats aside, before the read gives up.
const STREAM_QUIET: Duration = Duration::from_secs(120);

/// Sends `POST /` with `headers` and `body`, then reads the response until the door closes it.
fn read_to_end(addr: &str, headers: &[(&str, &str)], request_body: &str) -> Response {
    let mut stream = TcpStream::connect(addr).expect("should connect to the door");
    stream.set_read_timeout(Some(STREAM_QUIET)).ok();
    let mut head = format!("POST / HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{request_body}",
        request_body.len()
    ));
    stream.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .unwrap_or_else(|e| panic!("the stream did not end within {STREAM_QUIET:?} of quiet: {e}"));
    let raw = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no response head in {raw:?}"));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));
    let response_headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    super::wire_recorder::record_post(headers, request_body, status, &response_headers, body);
    Response {
        status,
        headers: response_headers,
        body: body.to_string(),
    }
}

/// A `stream/watch` connection read event by event, across as many reads as a test needs. Every
/// line it reads is checked against A2A v1.0 by the wire recorder.
pub struct Watch {
    stream: TcpStream,
    pending: Vec<u8>,
    /// The response head's lines, status line first, until the head is complete.
    head: Vec<String>,
    head_done: bool,
    /// The lines of the event being read.
    block: Vec<String>,
    /// Every event read so far.
    pub events: Vec<Event>,
    /// Whether the door has closed the connection.
    pub closed: bool,
}

impl Watch {
    /// Opens `stream/watch` resuming after `last_event_id`.
    pub fn open(addr: &str, last_event_id: u64) -> Self {
        Self {
            stream: super::idle_capsule::open_watch(addr, last_event_id),
            pending: Vec::new(),
            head: Vec::new(),
            head_done: false,
            block: Vec::new(),
            events: Vec::new(),
            closed: false,
        }
    }

    /// The response header `name` (lowercase), once the head has been read.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.iter().skip(1).find_map(|line| {
            let (header, value) = line.split_once(':')?;
            (header.trim().eq_ignore_ascii_case(name)).then(|| value.trim())
        })
    }

    /// Reads until `done` accepts an event read in this call, `timeout` passes or the door closes
    /// the connection. Returns whether `done` accepted one.
    pub fn read_until(&mut self, timeout: Duration, done: impl Fn(&Event) -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut buf = [0u8; 16 * 1024];
        let start = self.events.len();
        while !self.closed && std::time::Instant::now() < deadline {
            let read = (&self.stream).read(&mut buf);
            match read {
                Ok(0) => self.closed = true,
                Ok(n) => {
                    let now = std::time::Instant::now();
                    self.pending.extend_from_slice(&buf[..n]);
                    let mut lines = Vec::new();
                    while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
                        let raw: Vec<u8> = self.pending.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                            .trim_end_matches('\r')
                            .to_string();
                        lines.push((now, line));
                    }
                    super::wire_recorder::stream_lines(&self.stream, &lines);
                    for (_, line) in lines {
                        self.line(line);
                    }
                    if self.events[start..].iter().any(&done) {
                        return true;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => self.closed = true,
            }
        }
        self.events[start..].iter().any(done)
    }

    fn line(&mut self, line: String) {
        if !self.head_done {
            if line.is_empty() {
                self.head_done = true;
            } else {
                self.head.push(line);
            }
            return;
        }
        if !line.is_empty() {
            self.block.push(line);
            return;
        }
        let block = std::mem::take(&mut self.block);
        if block.is_empty() {
            return;
        }
        self.events
            .extend(parse(&format!("{}\n\n", block.join("\n"))));
    }
}
