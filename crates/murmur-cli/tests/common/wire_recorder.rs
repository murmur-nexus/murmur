//! Records every JSON-RPC exchange the common door helpers make and checks it against A2A v1.0
//! with [`super::a2a_conformance`], and against the one rule murmur adds to the proto's:
//! [`untimed_statuses`], every task status carries a `timestamp`.
//!
//! A request is measured only when the door accepted it: HTTP 200 with a JSON-RPC `result`, or
//! with an event stream. A refused request is a negative test, not a statement of what the door
//! serves, so only its JSON-RPC error response is envelope-checked; a non-200 status is not
//! measured at all.
//!
//! Any violation panics the thread that made the exchange, naming each violation's key, its
//! errors, the request and what the door answered.

use std::{
    collections::HashMap,
    net::{SocketAddr, TcpStream},
    sync::{Mutex, MutexGuard, OnceLock},
};

use serde_json::Value;

use super::a2a_conformance::{
    check_request, check_response, check_stream_event, resolve_method, Violation, WirePart,
};

/// The subject an [`untimed_statuses`] violation is reported under.
pub const UNTIMED_STATUS: &str = "murmur.TaskStatus.timestamp";

/// Every task status in `result` that carries no `timestamp`, as one violation of `request`'s
/// method under `part`, or none.
///
/// A task status is any object whose `state` is a string beginning `TASK_STATE_`, at any depth:
/// a task's `status`, a `statusUpdate`'s, a `ListTasks` page's. The proto leaves
/// `TaskStatus.timestamp` optional; murmur writes it on every status, so a status without one is
/// a status some writer forgot to stamp. Each miss is named by its JSON path.
pub fn untimed_statuses(request: &Value, part: WirePart, result: &Value) -> Vec<Violation> {
    let mut errors = Vec::new();
    collect_untimed(result, "result", &mut errors);
    if errors.is_empty() {
        return Vec::new();
    }
    let wire = request.get("method").and_then(Value::as_str).unwrap_or("-");
    let method = resolve_method(wire).map_or(wire, |resolved| resolved.name);
    vec![Violation {
        method: method.to_string(),
        part,
        subject: UNTIMED_STATUS.to_string(),
        errors,
    }]
}

fn collect_untimed(value: &Value, path: &str, errors: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            let is_status = object
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| state.starts_with("TASK_STATE_"));
            if is_status && !object.get("timestamp").is_some_and(Value::is_string) {
                errors.push(format!(
                    "{path}: a task status without a string `timestamp`"
                ));
            }
            for (key, member) in object {
                collect_untimed(member, &format!("{path}.{key}"), errors);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_untimed(item, &format!("{path}[{index}]"), errors);
            }
        }
        _ => {}
    }
}

/// [`untimed_statuses`] of one SSE event's `data`, when it is JSON carrying a `result`.
fn untimed_in_event(request: &Value, data: &str) -> Vec<Violation> {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|event| {
            event
                .get("result")
                .map(|result| untimed_statuses(request, WirePart::Event, result))
        })
        .unwrap_or_default()
}

/// The request header that carries the A2A protocol version; without it, v1.0 reads a request as
/// 0.3.
pub const VERSION_HEADER: &str = "A2A-Version";

/// A panic on one test thread must not hide every later exchange from the other threads.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One `POST /` exchange: the request's headers and body, the response's status, headers
/// (any case) and whole body. An event-stream body is read as SSE.
pub fn record_post(
    request_headers: &[(&str, &str)],
    request_body: &str,
    status: u16,
    response_headers: &[(String, String)],
    response_body: &str,
) {
    let Some(request) = json_rpc_request(request_body) else {
        return;
    };
    if status != 200 {
        return;
    }
    let mut violations = Vec::new();
    let context = if is_event_stream(response_headers) {
        violations.extend(accepted_request(&request, request_headers, true));
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        for line in sse_body_lines(response_body) {
            if let Some(data) = parser.line(line) {
                events.push(data);
            }
        }
        for data in &events {
            violations.extend(check_stream_event(&request, data));
            violations.extend(untimed_in_event(&request, data));
        }
        format!("events:\n{}", truncate(&events.join("\n")))
    } else {
        let Ok(response) = serde_json::from_str::<Value>(response_body) else {
            return;
        };
        violations.extend(check_response(&request, &response));
        if let Some(result) = response.get("result") {
            violations.extend(accepted_request(&request, request_headers, false));
            violations.extend(untimed_statuses(&request, WirePart::Result, result));
        }
        format!("response: {}", truncate(&response.to_string()))
    };
    enforce(violations, &request, &context);
}

/// Registers a `murmur.WatchSession` connection whose response [`stream_lines`] will read, keyed by
/// the socket's local address.
pub fn watch_opened(stream: &TcpStream, request_body: &str, request_headers: &[(&str, &str)]) {
    let (Some(request), Ok(local)) = (json_rpc_request(request_body), stream.local_addr()) else {
        return;
    };
    let headers = request_headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    lock(watches()).insert(
        local,
        Watch {
            request,
            request_headers: headers,
            phase: Phase::Status,
            response_headers: Vec::new(),
            parser: SseParser::default(),
        },
    );
}

/// Feeds every completed line read off `stream` to its connection's SSE parser and checks each
/// event it dispatches. A stream [`watch_opened`] did not register records nothing.
pub fn stream_lines<T>(stream: &TcpStream, lines: &[(T, String)]) {
    let Ok(local) = stream.local_addr() else {
        return;
    };
    let mut pending: Vec<(Vec<Violation>, Value, String)> = Vec::new();
    {
        let mut watches = lock(watches());
        let Some(watch) = watches.get_mut(&local) else {
            return;
        };
        for (_, line) in lines {
            match watch.phase {
                Phase::Ignored => break,
                Phase::Status => {
                    let status = line.split_whitespace().nth(1);
                    watch.phase = if status == Some("200") {
                        Phase::Head
                    } else {
                        Phase::Ignored
                    };
                }
                Phase::Head if line.is_empty() => {
                    if is_event_stream(&watch.response_headers) {
                        let headers: Vec<(&str, &str)> = watch
                            .request_headers
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_str()))
                            .collect();
                        let violations = accepted_request(&watch.request, &headers, true);
                        pending.push((violations, watch.request.clone(), String::new()));
                        watch.phase = Phase::Body;
                    } else {
                        // A JSON answer arrives without a final newline, so its body never
                        // completes a line here.
                        watch.phase = Phase::Ignored;
                    }
                }
                Phase::Head => {
                    if let Some((name, value)) = line.split_once(':') {
                        watch
                            .response_headers
                            .push((name.trim().to_string(), value.trim().to_string()));
                    }
                }
                Phase::Body => {
                    if let Some(data) = watch.parser.line(line) {
                        let mut violations = check_stream_event(&watch.request, &data);
                        violations.extend(untimed_in_event(&watch.request, &data));
                        pending.push((
                            violations,
                            watch.request.clone(),
                            format!("event: {}", truncate(&data)),
                        ));
                    }
                }
            }
        }
    }
    for (violations, request, context) in pending {
        enforce(violations, &request, &context);
    }
}

struct Watch {
    request: Value,
    request_headers: Vec<(String, String)>,
    phase: Phase,
    response_headers: Vec<(String, String)>,
    parser: SseParser,
}

#[derive(Clone, Copy)]
enum Phase {
    Status,
    Head,
    Body,
    Ignored,
}

fn watches() -> &'static Mutex<HashMap<SocketAddr, Watch>> {
    static WATCHES: OnceLock<Mutex<HashMap<SocketAddr, Watch>>> = OnceLock::new();
    WATCHES.get_or_init(Default::default)
}

/// The body as a JSON object with a `method`, which is what makes it a JSON-RPC request.
fn json_rpc_request(body: &str) -> Option<Value> {
    serde_json::from_str::<Value>(body)
        .ok()
        .filter(|request| request.get("method").is_some())
}

fn is_event_stream(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value
                .trim()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
    })
}

/// What an accepted request raises beyond its response: [`check_request`], a missing
/// `A2A-Version: 1.0` header (or `1.0.<patch>`), and a unary method answered with an event stream.
fn accepted_request(request: &Value, headers: &[(&str, &str)], streamed: bool) -> Vec<Violation> {
    let mut violations = check_request(request);
    let wire = request.get("method").and_then(Value::as_str).unwrap_or("-");
    let resolved = resolve_method(wire);
    let method = resolved.map_or(wire, |resolved| resolved.name);
    let version = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(VERSION_HEADER))
        .map(|(_, value)| value.trim());
    // A patch number takes no part in negotiation (A2A v1.0.1 specification §3.6).
    let is_1_0 = |value: &str| {
        value == "1.0"
            || value
                .strip_prefix("1.0.")
                .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
    };
    if !version.is_some_and(is_1_0) {
        let found = version.map_or("no such header".to_string(), |value| format!("{value:?}"));
        violations.push(Violation {
            method: method.to_string(),
            part: WirePart::Header,
            subject: VERSION_HEADER.to_string(),
            errors: vec![format!(
                "accepted without `{VERSION_HEADER}: 1.0` ({found}), which v1.0 reads as an A2A 0.3 request"
            )],
        });
    }
    if streamed && resolved.is_some_and(|resolved| !resolved.streaming) {
        violations.push(Violation {
            method: wire.to_string(),
            part: WirePart::Envelope,
            subject: "-".to_string(),
            errors: vec![format!(
                "`{wire}` is unary but was answered with text/event-stream"
            )],
        });
    }
    violations
}

/// Panics when `violations` is not empty: the first key leads the message, each later one
/// follows as `and <key>`, every key with its errors, then the request and `context`.
fn enforce(violations: Vec<Violation>, request: &Value, context: &str) {
    if violations.is_empty() {
        return;
    }
    let mut message = String::new();
    for (index, violation) in violations.iter().enumerate() {
        let lead = if index == 0 {
            "A2A v1.0 wire check failed: "
        } else {
            "and "
        };
        message.push_str(&format!("{lead}{}\n", violation.key()));
        for error in &violation.errors {
            message.push_str(&format!("  - {error}\n"));
        }
    }
    message.push_str(&format!(
        "request: {}\n{context}",
        truncate(&request.to_string())
    ));
    panic!("{message}");
}

/// The longest stretch of a request, response or event a failure message quotes.
const QUOTE_LIMIT: usize = 4096;

fn truncate(text: &str) -> String {
    if text.len() <= QUOTE_LIMIT {
        return text.to_string();
    }
    let mut end = QUOTE_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes in all)", &text[..end], text.len())
}

/// The lines of an SSE response after its HTTP head, when `body` still carries one, else all of
/// them.
fn sse_body_lines(body: &str) -> impl Iterator<Item = &str> {
    let lines: Vec<&str> = body
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let start = if lines.first().is_some_and(|line| line.starts_with("HTTP/")) {
        lines
            .iter()
            .position(|line| line.is_empty())
            .map_or(lines.len(), |blank| blank + 1)
    } else {
        0
    };
    lines.into_iter().skip(start)
}

/// The SSE event-stream grammar, as far as `data` goes: `:` lines are comments, `data:` lines
/// join with `\n` (one leading space stripped), and a blank line dispatches an event that has
/// data. `id:` and `event:` lines are not judged, and an event the stream ends before its blank
/// line is discarded, as an SSE client discards it.
#[derive(Default)]
struct SseParser {
    data: Option<String>,
}

impl SseParser {
    fn line(&mut self, line: &str) -> Option<String> {
        if line.is_empty() {
            return self.data.take();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_string()),
            }
        }
        None
    }
}
