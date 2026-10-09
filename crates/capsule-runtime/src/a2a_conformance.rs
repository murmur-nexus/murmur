//! Strict checks of A2A v1.0 wire traffic against the vendored upstream `a2a.proto`: any message
//! of package `lf.a2a.v1` under protobuf JSON, JSON-RPC request and response envelopes, and SSE
//! stream events.
//!
//! The oracle is the proto itself rather than a hand-written mirror of it: the message, enum and
//! service blocks are read out of the pinned file, so the checks follow the normative definition
//! and nothing else. A value is judged the way a strict protobuf JSON parser judges it — a key
//! that is not the JSON name of a field is an error, and so is a field the proto marks
//! `(google.api.field_behavior) = REQUIRED` that the value omits. On top of bare protobuf JSON,
//! every `oneof` must have exactly one member set: each oneof in `a2a.proto` is a discriminated
//! union whose empty form carries nothing.
//!
//! The JSON-RPC binding names its methods after the gRPC rpcs (A2A v1.0.1 specification §9.1), so
//! the method table is the proto's `service A2AService` block, read the same way.
//!
//! This file uses only `std` and `serde_json` and no `crate::` path, because the `murmur-cli`
//! integration tests compile it too, through `#[path]` from `tests/common/mod.rs`. The
//! `include_str!` below resolves relative to this file, so it finds the proto from either crate.
//!
//! The reader covers the proto subset `a2a.proto` is written in: one field per line, top-level
//! `message`, `enum` and `service` blocks, one-line `rpc` headers, `oneof` groups, `map<K, V>`
//! fields, and nested `enum` blocks. A nested `message` is reported as a checker error rather than
//! guessed at. No check panics, whatever the input.

use std::{collections::HashMap, sync::OnceLock};

use serde_json::Value;

/// `a2a.proto` at tag `v1.0.1`, verbatim. See the README beside it for its provenance.
const A2A_PROTO: &str = include_str!("../tests/fixtures/a2a/v1.0.1/a2a.proto");

/// The message a served agent card is.
const AGENT_CARD_MESSAGE: &str = "lf.a2a.v1.AgentCard";

/// The message every SSE event of a streaming method carries as its JSON-RPC `result`.
const STREAM_RESPONSE_MESSAGE: &str = "lf.a2a.v1.StreamResponse";

/// The door's A2A 0.3 method names and the v1.0 method each one is measured as. Every request
/// under one of these names is a `method` violation of its own.
pub(crate) const LEGACY_METHOD_NAMES: [(&str, &str); 5] = [
    ("message/send", "SendMessage"),
    ("message/stream", "SendStreamingMessage"),
    ("tasks/get", "GetTask"),
    ("tasks/cancel", "CancelTask"),
    ("agent/getAuthenticatedExtendedCard", "GetExtendedAgentCard"),
];

/// The murmur extension methods the door serves beside the A2A ones, each with whether it
/// streams. Their envelopes and stream events are checked; their params and unary results are
/// murmur's own and are not.
pub(crate) const EXTENSION_METHODS: [(&str, bool); 2] =
    [("stream/watch", true), ("session/stop", false)];

/// Every error a strict protobuf JSON parse of `card` as `lf.a2a.v1.AgentCard` would raise.
pub(crate) fn check_agent_card(card: &Value) -> Result<(), Vec<String>> {
    check_message(card, AGENT_CARD_MESSAGE)
}

/// Every error a strict protobuf JSON parse of `value` as `message` would raise, each naming the
/// JSON path it is about (`capabilities.extensions[1].params`).
///
/// `message` is qualified with the proto's package (`lf.a2a.v1.Task`); a name in any other
/// package, or one the proto does not define, is an error. A proto the reader cannot follow is
/// reported the same way, as an error, never as a panic.
pub(crate) fn check_message(value: &Value, message: &str) -> Result<(), Vec<String>> {
    let proto = proto().map_err(|error| vec![error])?;
    let Some(short) = proto.short_message_name(message) else {
        return Err(vec![format!("a2a.proto defines no message `{message}`")]);
    };
    let mut errors = Vec::new();
    proto.check_message(value, short, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// One rpc of `service A2AService`, which is also one JSON-RPC method of the v1.0 binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodSpec {
    /// The method name, `SendMessage`.
    pub(crate) name: String,
    /// The request message, qualified: `lf.a2a.v1.SendMessageRequest`.
    pub(crate) params: String,
    /// The response message, qualified; a `google.protobuf.*` type stays as the proto writes it.
    pub(crate) result: String,
    /// The rpc returns a `stream`: its results travel as SSE events, one `result` each.
    pub(crate) streaming: bool,
}

/// The v1.0 methods, in the order `service A2AService` declares them.
pub(crate) fn v1_methods() -> Result<&'static [MethodSpec], String> {
    proto().map(|proto| proto.methods.as_slice())
}

/// The part of an exchange a [`Violation`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WirePart {
    /// The request's method name.
    Method,
    /// A request header, `A2A-Version`.
    Header,
    /// The JSON-RPC request or response envelope.
    Envelope,
    /// The request's `params` as the method's request message.
    Params,
    /// A unary response's `result` as the method's response message.
    Result,
    /// One SSE event of a streaming response.
    Event,
}

impl WirePart {
    pub(crate) const ALL: [WirePart; 6] = [
        WirePart::Method,
        WirePart::Header,
        WirePart::Envelope,
        WirePart::Params,
        WirePart::Result,
        WirePart::Event,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WirePart::Method => "method",
            WirePart::Header => "header",
            WirePart::Envelope => "envelope",
            WirePart::Params => "params",
            WirePart::Result => "result",
            WirePart::Event => "event",
        }
    }

    /// The part `spelling` names, as [`WirePart::as_str`] spells it.
    pub(crate) fn parse(spelling: &str) -> Option<WirePart> {
        WirePart::ALL
            .into_iter()
            .find(|part| part.as_str() == spelling)
    }
}

/// Every error one part of one exchange raised, under one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Violation {
    /// The v1.0 method name (or the extension name) for `header`, `params`, `result` and `event`;
    /// the name as sent on the wire for `method` and `envelope`. `-` when the request carries no
    /// string method.
    pub(crate) method: String,
    pub(crate) part: WirePart,
    /// The qualified message for `params`, `result` and `event`, the header name for `header`,
    /// and `-` for `method` and `envelope`.
    pub(crate) subject: String,
    pub(crate) errors: Vec<String>,
}

impl Violation {
    /// `<method> <part> <subject>`: one line's key in the wire exceptions list.
    pub(crate) fn key(&self) -> String {
        format!("{} {} {}", self.method, self.part.as_str(), self.subject)
    }
}

/// What a request's `method` string names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodKind {
    /// A v1.0 method under its own name.
    V1,
    /// A v1.0 method under its A2A 0.3 name, from [`LEGACY_METHOD_NAMES`].
    Legacy,
    /// A murmur extension method, from [`EXTENSION_METHODS`].
    Extension,
}

/// The method a request's `method` string is measured as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedMethod {
    pub(crate) kind: MethodKind,
    /// The v1.0 name, or the extension name for an extension method.
    pub(crate) name: &'static str,
    pub(crate) streaming: bool,
    /// The v1.0 rpc; `None` for an extension method.
    pub(crate) spec: Option<&'static MethodSpec>,
}

/// The method `wire_name` is measured as, or `None` when it is neither a v1.0 method, an A2A 0.3
/// name of one, nor a murmur extension method.
pub(crate) fn resolve_method(wire_name: &str) -> Option<ResolvedMethod> {
    let methods = v1_methods().ok()?;
    let find = |name: &str| methods.iter().find(|spec| spec.name == name);
    if let Some(spec) = find(wire_name) {
        return Some(ResolvedMethod {
            kind: MethodKind::V1,
            name: spec.name.as_str(),
            streaming: spec.streaming,
            spec: Some(spec),
        });
    }
    if let Some((_, v1_name)) = LEGACY_METHOD_NAMES
        .iter()
        .find(|(legacy, _)| *legacy == wire_name)
    {
        let spec = find(v1_name)?;
        return Some(ResolvedMethod {
            kind: MethodKind::Legacy,
            name: spec.name.as_str(),
            streaming: spec.streaming,
            spec: Some(spec),
        });
    }
    EXTENSION_METHODS
        .iter()
        .find(|(name, _)| *name == wire_name)
        .map(|&(name, streaming)| ResolvedMethod {
            kind: MethodKind::Extension,
            name,
            streaming,
            spec: None,
        })
}

/// Every violation a JSON-RPC request raises: its envelope, its method name, and, for a v1.0
/// method or a 0.3 name of one, its `params` as the method's request message. Absent `params`
/// are checked as `{}`. An extension method's params are not measured.
pub(crate) fn check_request(request: &Value) -> Vec<Violation> {
    let wire = wire_method(request);
    let mut violations = Vec::new();
    let Some(object) = request.as_object() else {
        push(
            &mut violations,
            &wire,
            WirePart::Envelope,
            "-",
            vec![format!(
                "the request must be a JSON object, found {}",
                kind(request)
            )],
        );
        return violations;
    };

    let mut envelope = Vec::new();
    if object.get("jsonrpc") != Some(&Value::from("2.0")) {
        envelope.push(format!(
            "`jsonrpc` must be \"2.0\", found {}",
            shown(object.get("jsonrpc"))
        ));
    }
    match object.get("id") {
        Some(Value::String(_)) => {}
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {}
        other => envelope.push(format!(
            "`id` must be a string or an integer, found {}",
            shown(other)
        )),
    }
    if !matches!(object.get("method"), Some(Value::String(_))) {
        envelope.push(format!(
            "`method` must be a string, found {}",
            shown(object.get("method"))
        ));
    }
    let params = object.get("params");
    if let Some(params) = params.filter(|params| !params.is_object()) {
        envelope.push(format!(
            "`params` must be a JSON object, found {}",
            kind(params)
        ));
    }
    for key in object.keys() {
        if !["jsonrpc", "id", "method", "params"].contains(&key.as_str()) {
            envelope.push(format!("unknown request key `{key}`"));
        }
    }
    push(&mut violations, &wire, WirePart::Envelope, "-", envelope);

    let Some(Value::String(method)) = object.get("method") else {
        return violations;
    };
    let Some(resolved) = resolve_method(method) else {
        push(
            &mut violations,
            method,
            WirePart::Method,
            "-",
            vec![format!(
                "`{method}` is not a v1.0 method or a declared murmur extension method"
            )],
        );
        return violations;
    };
    if resolved.kind == MethodKind::Legacy {
        push(
            &mut violations,
            method,
            WirePart::Method,
            "-",
            vec![format!(
                "`{method}` is the A2A 0.3 name; v1.0 calls it `{}`",
                resolved.name
            )],
        );
    }
    let empty = Value::Object(Default::default());
    // Non-object params are already an envelope violation; there is no message to check.
    let measured = match params {
        None => Some(&empty),
        Some(params) => Some(params).filter(|params| params.is_object()),
    };
    if let (Some(spec), Some(params)) = (resolved.spec, measured) {
        let errors = check_named(params, &spec.params, "params");
        push(
            &mut violations,
            resolved.name,
            WirePart::Params,
            &spec.params,
            errors,
        );
    }
    violations
}

/// Every violation a JSON-RPC response to `request` raises: its envelope, and, for a unary v1.0
/// method or a 0.3 name of one, its `result` as the method's response message. A `result` on a
/// streaming method is an envelope violation. An extension method's result is not measured.
pub(crate) fn check_response(request: &Value, response: &Value) -> Vec<Violation> {
    let wire = wire_method(request);
    let mut violations = Vec::new();
    let mut envelope = envelope_errors(request, response);
    let resolved = resolve_method(&wire);
    let result = response.get("result");
    if let (Some(resolved), Some(_)) = (resolved, result) {
        if resolved.streaming {
            envelope.push(format!(
                "`{wire}` streams: its results travel as SSE events, not as one JSON `result`"
            ));
        }
    }
    push(&mut violations, &wire, WirePart::Envelope, "-", envelope);

    if let (Some(resolved), Some(result)) = (resolved, result) {
        if let Some(spec) = resolved.spec.filter(|spec| !spec.streaming) {
            let errors = check_named(result, &spec.result, "result");
            push(
                &mut violations,
                resolved.name,
                WirePart::Result,
                &spec.result,
                errors,
            );
        }
    }
    violations
}

/// Every violation one SSE event's `data` raises on a stream answering `request`: it must be a
/// JSON-RPC response carrying the request's `id`, under the same envelope rules as
/// [`check_response`], whose `result` is a `StreamResponse`. An `error` event is checked as an
/// envelope only. Every error is reported under part `event`.
pub(crate) fn check_stream_event(request: &Value, data: &str) -> Vec<Violation> {
    let wire = wire_method(request);
    let method = resolve_method(&wire).map_or(wire.as_str(), |resolved| resolved.name);
    let errors = match serde_json::from_str::<Value>(data) {
        Err(error) => vec![format!("the event data is not JSON: {error}")],
        Ok(event) => {
            let mut errors = envelope_errors(request, &event);
            if let Some(result) = event.get("result") {
                errors.extend(check_named(result, STREAM_RESPONSE_MESSAGE, "result"));
            }
            errors
        }
    };
    let mut violations = Vec::new();
    push(
        &mut violations,
        method,
        WirePart::Event,
        STREAM_RESPONSE_MESSAGE,
        errors,
    );
    violations
}

/// The request's `method` as sent, or `-` when it carries no string method.
fn wire_method(request: &Value) -> String {
    request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

fn push(
    violations: &mut Vec<Violation>,
    method: &str,
    part: WirePart,
    subject: &str,
    errors: Vec<String>,
) {
    if !errors.is_empty() {
        violations.push(Violation {
            method: method.to_string(),
            part,
            subject: subject.to_string(),
            errors,
        });
    }
}

/// The JSON-RPC error codes outside the A2A range (A2A v1.0.1 specification §9.5).
const STANDARD_ERROR_CODES: [i64; 5] = [-32700, -32600, -32601, -32602, -32603];

/// The A2A-specific JSON-RPC error codes, inclusive.
const A2A_ERROR_CODES: std::ops::RangeInclusive<i64> = -32099..=-32001;

/// The codes a response may answer with a `null` id: the request could not be read far enough to
/// learn its id.
const NULL_ID_ERROR_CODES: [i64; 2] = [-32700, -32600];

/// Every envelope error of a JSON-RPC response to `request`.
fn envelope_errors(request: &Value, response: &Value) -> Vec<String> {
    let Some(object) = response.as_object() else {
        return vec![format!(
            "the response must be a JSON object, found {}",
            kind(response)
        )];
    };
    let mut errors = Vec::new();
    if object.get("jsonrpc") != Some(&Value::from("2.0")) {
        errors.push(format!(
            "`jsonrpc` must be \"2.0\", found {}",
            shown(object.get("jsonrpc"))
        ));
    }
    for key in object.keys() {
        if !["jsonrpc", "id", "result", "error"].contains(&key.as_str()) {
            errors.push(format!("unknown response key `{key}`"));
        }
    }
    let error_code = object
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64);
    let expected_id = request.get("id").unwrap_or(&Value::Null);
    match object.get("id") {
        None => errors.push("missing `id`".to_string()),
        Some(Value::Null) => {
            if !error_code.is_some_and(|code| NULL_ID_ERROR_CODES.contains(&code)) {
                errors.push(format!(
                    "`id` is null, which only an error {} or {} may answer; the request's id is {expected_id}",
                    NULL_ID_ERROR_CODES[0], NULL_ID_ERROR_CODES[1]
                ));
            }
        }
        Some(id) if id == expected_id => {}
        Some(id) => errors.push(format!("`id` is {id}, not the request's id {expected_id}")),
    }
    match (object.get("result"), object.get("error")) {
        (Some(_), Some(_)) => errors.push("sets both `result` and `error`".to_string()),
        (None, None) => errors.push("sets neither `result` nor `error`".to_string()),
        (None, Some(error)) => errors.extend(error_object_errors(error)),
        (Some(_), None) => {}
    }
    errors
}

/// Every error in a JSON-RPC `error` object.
fn error_object_errors(error: &Value) -> Vec<String> {
    let Some(object) = error.as_object() else {
        return vec![format!(
            "`error` must be a JSON object, found {}",
            kind(error)
        )];
    };
    let mut errors = Vec::new();
    match object.get("code") {
        Some(Value::Number(number)) if number.is_i64() => {
            let code = number.as_i64().unwrap_or_default();
            if !STANDARD_ERROR_CODES.contains(&code) && !A2A_ERROR_CODES.contains(&code) {
                errors.push(format!(
                    "`error.code` {code} is neither a standard JSON-RPC code nor in the A2A range {}..={}",
                    A2A_ERROR_CODES.start(),
                    A2A_ERROR_CODES.end()
                ));
            }
        }
        other => errors.push(format!(
            "`error.code` must be an integer, found {}",
            shown(other)
        )),
    }
    if !matches!(object.get("message"), Some(Value::String(_))) {
        errors.push(format!(
            "`error.message` must be a string, found {}",
            shown(object.get("message"))
        ));
    }
    match object.get("data") {
        None => {}
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                if !matches!(item.get("@type"), Some(Value::String(_))) {
                    errors.push(format!(
                        "`error.data[{index}]` must be an object with a string `@type`"
                    ));
                }
            }
        }
        Some(other) => errors.push(format!(
            "`error.data` must be an array of `@type` objects, found {}",
            kind(other)
        )),
    }
    for key in object.keys() {
        if !["code", "message", "data"].contains(&key.as_str()) {
            errors.push(format!("unknown error key `error.{key}`"));
        }
    }
    errors
}

/// Every error of `value` as `type_name`, a qualified `lf.a2a.v1` message or a
/// `google.protobuf.*` type, its paths starting at `path`.
fn check_named(value: &Value, type_name: &str, path: &str) -> Vec<String> {
    let proto = match proto() {
        Ok(proto) => proto,
        Err(error) => return vec![error],
    };
    let mut errors = Vec::new();
    match proto.short_message_name(type_name) {
        Some(short) => proto.check_message(value, short, path, &mut errors),
        None => proto.check_type(value, type_name, path, &mut errors),
    }
    errors
}

/// The parsed proto, read once per process.
fn proto() -> Result<&'static Proto, String> {
    static PROTO: OnceLock<Result<Proto, String>> = OnceLock::new();
    PROTO
        .get_or_init(|| Proto::parse(A2A_PROTO).map_err(|error| format!("a2a.proto: {error}")))
        .as_ref()
        .map_err(Clone::clone)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Singular,
    Optional,
    Repeated,
}

#[derive(Debug)]
struct Field {
    proto_name: String,
    json_name: String,
    label: Label,
    /// The type as written: a scalar, a message or enum name, `map<K, V>`, or a
    /// `google.protobuf.*` well-known type.
    type_name: String,
    required: bool,
    /// The `oneof` group the field belongs to; exactly one member of a group must be set.
    oneof: Option<String>,
}

#[derive(Debug, Default)]
struct Proto {
    package: String,
    messages: HashMap<String, Vec<Field>>,
    /// Enum name → its values, each a name and its number.
    enums: HashMap<String, Vec<(String, i64)>>,
    methods: Vec<MethodSpec>,
}

/// What a block the reader has entered is, so its closing brace ends the right thing.
enum Block {
    Message(String),
    Oneof(String),
    Enum(String),
    Service(String),
}

impl Proto {
    fn parse(source: &str) -> Result<Proto, String> {
        let mut proto = Proto::default();
        let mut blocks: Vec<Block> = Vec::new();
        // Depth of a block whose contents are not read (top-level options, rpc option bodies):
        // only its braces are counted, until it closes.
        let mut skipped_depth = 0usize;

        for (index, raw_line) in source.lines().enumerate() {
            let line_no = index + 1;
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            let (opens, closes) = count_braces(line);

            if skipped_depth > 0 {
                skipped_depth = (skipped_depth + opens)
                    .checked_sub(closes)
                    .ok_or_else(|| format!("line {line_no}: unbalanced '}}'"))?;
                continue;
            }

            let words: Vec<&str> = line.split_whitespace().collect();
            if matches!(words.first().copied(), Some("message" | "enum" | "oneof"))
                && opens > 0
                && opens == closes
            {
                // A block opened and closed on one line, `message Empty {}`: it declares a name
                // and no fields.
                let name = block_name(&words, line_no)?;
                match words[0] {
                    "message" => {
                        proto.messages.entry(name).or_default();
                    }
                    "enum" => {
                        proto.enums.entry(name).or_default();
                    }
                    _ => {}
                }
                continue;
            }
            match blocks.last() {
                None => match words.first().copied() {
                    Some("message") => {
                        let name = block_name(&words, line_no)?;
                        proto.messages.entry(name.clone()).or_default();
                        blocks.push(Block::Message(name));
                    }
                    Some("enum") => {
                        let name = block_name(&words, line_no)?;
                        proto.enums.entry(name.clone()).or_default();
                        blocks.push(Block::Enum(name));
                    }
                    Some("service") => {
                        let name = block_name(&words, line_no)?;
                        blocks.push(Block::Service(name));
                    }
                    Some("package") => {
                        proto.package = words
                            .get(1)
                            .map(|package| package.trim_end_matches(';').to_string())
                            .filter(|package| !package.is_empty())
                            .ok_or_else(|| format!("line {line_no}: a package with no name"))?;
                    }
                    Some("option") => skipped_depth = opens.saturating_sub(closes),
                    Some("syntax" | "import") => {}
                    _ => return Err(format!("line {line_no}: unexpected `{line}`")),
                },
                Some(Block::Service(_)) => match words.first().copied() {
                    _ if line == "}" => {
                        blocks.pop();
                    }
                    Some("rpc") => {
                        let method = parse_rpc(line, &proto.package)
                            .map_err(|error| format!("line {line_no}: {error}"))?;
                        proto.methods.push(method);
                        skipped_depth = opens.saturating_sub(closes);
                    }
                    Some("option") => skipped_depth = opens.saturating_sub(closes),
                    _ => return Err(format!("line {line_no}: unexpected `{line}` in a service")),
                },
                Some(Block::Enum(name)) => match words.first().copied() {
                    _ if line == "}" => {
                        blocks.pop();
                    }
                    Some("option" | "reserved") => skipped_depth = opens.saturating_sub(closes),
                    Some(value) if line.contains('=') => {
                        let number = enum_number(line).ok_or_else(|| {
                            format!("line {line_no}: `{line}` has no value number")
                        })?;
                        let name = name.clone();
                        proto
                            .enums
                            .entry(name)
                            .or_default()
                            .push((value.to_string(), number));
                    }
                    _ => return Err(format!("line {line_no}: unexpected `{line}` in an enum")),
                },
                Some(Block::Message(_) | Block::Oneof(_)) => {
                    if line == "}" {
                        blocks.pop();
                        continue;
                    }
                    match words.first().copied() {
                        Some("reserved" | "option") => {
                            skipped_depth = opens.saturating_sub(closes);
                        }
                        Some("oneof") => {
                            let name = block_name(&words, line_no)?;
                            blocks.push(Block::Oneof(name));
                        }
                        Some("enum") => {
                            let name = block_name(&words, line_no)?;
                            proto.enums.entry(name.clone()).or_default();
                            blocks.push(Block::Enum(name));
                        }
                        Some("message") => {
                            return Err(format!(
                                "line {line_no}: nested messages are not supported by this reader"
                            ))
                        }
                        _ => {
                            let (message, oneof) = enclosing(&blocks).ok_or_else(|| {
                                format!("line {line_no}: field outside a message")
                            })?;
                            let field = parse_field(line, oneof.cloned())
                                .map_err(|error| format!("line {line_no}: {error}"))?;
                            let message = message.clone();
                            proto.messages.entry(message).or_default().push(field);
                        }
                    }
                }
            }
        }
        if let Some(open) = blocks.last() {
            let name = match open {
                Block::Message(name)
                | Block::Oneof(name)
                | Block::Enum(name)
                | Block::Service(name) => name,
            };
            return Err(format!("block `{name}` is never closed"));
        }
        if proto.package.is_empty() {
            return Err("no `package` line".to_string());
        }
        Ok(proto)
    }

    /// The unqualified name of `qualified` when it is a message of this proto's package.
    fn short_message_name<'a>(&self, qualified: &'a str) -> Option<&'a str> {
        qualified
            .strip_prefix(self.package.as_str())
            .and_then(|rest| rest.strip_prefix('.'))
            .filter(|short| self.messages.contains_key(*short))
    }

    fn check_message(&self, value: &Value, message: &str, path: &str, errors: &mut Vec<String>) {
        let Some(fields) = self.messages.get(message) else {
            errors.push(format!(
                "`{}`: a2a.proto defines no message `{message}`",
                display(path, message)
            ));
            return;
        };
        let Some(object) = value.as_object() else {
            errors.push(format!(
                "`{}` must be a JSON object (message {message}), found {}",
                display(path, message),
                kind(value)
            ));
            return;
        };

        for key in object.keys() {
            if fields.iter().any(|field| &field.json_name == key) {
                continue;
            }
            match fields.iter().find(|field| &field.proto_name == key) {
                Some(field) => errors.push(format!(
                    "unknown field `{}`: the JSON name of proto field `{key}` is `{}`",
                    join(path, key),
                    field.json_name
                )),
                None => errors.push(format!("unknown field `{}`", join(path, key))),
            }
        }

        // Each oneof group in declaration order, with the members set.
        let mut oneofs: Vec<(&str, Vec<&str>)> = Vec::new();
        for field in fields {
            if let Some(group) = &field.oneof {
                if !oneofs.iter().any(|(name, _)| name == group) {
                    oneofs.push((group.as_str(), Vec::new()));
                }
            }
        }
        for field in fields {
            let field_path = join(path, &field.json_name);
            // A `null` means unset, except in a singular `google.protobuf.Value`, where it is the
            // JSON null the field holds.
            let holds_null =
                field.type_name == "google.protobuf.Value" && field.label != Label::Repeated;
            match object.get(&field.json_name) {
                None => self.report_missing(field, &field_path, errors),
                Some(Value::Null) if !holds_null => self.report_missing(field, &field_path, errors),
                Some(present) => {
                    if let Some(group) = &field.oneof {
                        if let Some((_, members)) =
                            oneofs.iter_mut().find(|(name, _)| name == group)
                        {
                            members.push(field.json_name.as_str());
                        }
                    }
                    self.check_field(present, field, &field_path, errors);
                    if field.required && field.label != Label::Repeated {
                        if let Some(zero) = self.enum_zero_name(&field.type_name, present) {
                            errors.push(format!(
                                "required field `{field_path}` holds its zero value `{zero}`"
                            ));
                        }
                    }
                }
            }
        }
        for (group, members) in oneofs {
            if members.is_empty() {
                errors.push(format!(
                    "`{}` sets no member of oneof `{group}`",
                    display(path, message)
                ));
            } else if members.len() > 1 {
                errors.push(format!(
                    "`{}` sets more than one member of oneof `{group}`: {}",
                    display(path, message),
                    members.join(", ")
                ));
            }
        }
    }

    fn report_missing(&self, field: &Field, path: &str, errors: &mut Vec<String>) {
        if field.required {
            errors.push(format!("missing required field `{path}`"));
        }
    }

    /// The name of `value` when `type_name` is an enum and `value` names its 0 value.
    fn enum_zero_name(&self, type_name: &str, value: &Value) -> Option<String> {
        let values = self.enums.get(short_type(type_name))?;
        let name = value.as_str()?;
        values
            .iter()
            .find(|(value_name, number)| value_name == name && *number == 0)
            .map(|(value_name, _)| value_name.clone())
    }

    fn check_field(&self, value: &Value, field: &Field, path: &str, errors: &mut Vec<String>) {
        if let Some((key_type, value_type)) = map_types(&field.type_name) {
            let Some(entries) = value.as_object() else {
                errors.push(format!(
                    "`{path}` must be a JSON object (map), found {}",
                    kind(value)
                ));
                return;
            };
            for (key, entry) in entries {
                if key_type != "string" && key_type != "bool" && key.parse::<i128>().is_err() {
                    errors.push(format!("`{path}` has key {key:?}, not a {key_type}"));
                }
                self.check_type(entry, value_type, &format!("{path}[{key:?}]"), errors);
            }
            return;
        }
        if field.label == Label::Repeated {
            let Some(items) = value.as_array() else {
                errors.push(format!(
                    "`{path}` must be a JSON array (repeated {}), found {}",
                    field.type_name,
                    kind(value)
                ));
                return;
            };
            for (index, item) in items.iter().enumerate() {
                self.check_type(item, &field.type_name, &format!("{path}[{index}]"), errors);
            }
            return;
        }
        self.check_type(value, &field.type_name, path, errors);
    }

    fn check_type(&self, value: &Value, type_name: &str, path: &str, errors: &mut Vec<String>) {
        let fits = match type_name {
            "string" => value.is_string(),
            "bytes" => {
                if let Value::String(text) = value {
                    if !is_base64(text) {
                        errors.push(format!("`{path}` is not base64 (bytes)"));
                    }
                    return;
                }
                false
            }
            "bool" => value.is_boolean(),
            "int32" | "int64" | "uint32" | "uint64" | "sint32" | "sint64" | "fixed32"
            | "fixed64" | "sfixed32" | "sfixed64" => {
                let integer = match value {
                    Value::Number(number) => number
                        .as_i64()
                        .map(i128::from)
                        .or_else(|| number.as_u64().map(i128::from)),
                    Value::String(text) => text.parse::<i128>().ok(),
                    _ => None,
                };
                if let Some(integer) = integer {
                    if !integer_range(type_name).contains(&integer) {
                        errors.push(format!(
                            "`{path}` is {integer}, out of range for {type_name}"
                        ));
                    }
                    return;
                }
                false
            }
            "float" | "double" => match value {
                Value::Number(_) => true,
                Value::String(text) => {
                    matches!(text.as_str(), "NaN" | "Infinity" | "-Infinity")
                        || text.parse::<f64>().is_ok()
                }
                _ => false,
            },
            "google.protobuf.Struct" => value.is_object(),
            "google.protobuf.ListValue" => value.is_array(),
            "google.protobuf.Value" => true,
            "google.protobuf.Empty" => value.as_object().is_some_and(|object| object.is_empty()),
            "google.protobuf.Timestamp" => {
                if let Value::String(text) = value {
                    if !is_rfc3339(text) {
                        errors.push(format!("`{path}` is {text:?}, not an RFC 3339 timestamp"));
                    }
                    return;
                }
                false
            }
            _ => {
                let short = short_type(type_name);
                if let Some(values) = self.enums.get(short) {
                    match value {
                        Value::String(name) if values.iter().any(|(value, _)| value == name) => {}
                        Value::String(name) => {
                            errors.push(format!("`{path}` is {name:?}, not a value of {short}"))
                        }
                        Value::Number(number) => errors.push(format!(
                            "`{path}` is {number}: enum {short} takes a value name, not an integer"
                        )),
                        _ => errors.push(format!(
                            "`{path}` must be a {short} value, found {}",
                            kind(value)
                        )),
                    }
                } else if self.messages.contains_key(short) {
                    self.check_message(value, short, path, errors);
                } else {
                    errors.push(format!("`{path}`: a2a.proto defines no type `{type_name}`"));
                }
                return;
            }
        };
        if !fits {
            let found = match (type_name, value) {
                ("google.protobuf.Empty", Value::Object(_)) => "an object with fields",
                _ => kind(value),
            };
            errors.push(format!("`{path}` must be a {type_name}, found {found}"));
        }
    }
}

/// The values an integer type holds.
fn integer_range(type_name: &str) -> std::ops::RangeInclusive<i128> {
    match type_name {
        "int32" | "sint32" | "sfixed32" => i128::from(i32::MIN)..=i128::from(i32::MAX),
        "uint32" | "fixed32" => 0..=i128::from(u32::MAX),
        "uint64" | "fixed64" => 0..=i128::from(u64::MAX),
        _ => i128::from(i64::MIN)..=i128::from(i64::MAX),
    }
}

/// A protobuf JSON `bytes` value: standard or URL-safe base64, padded or unpadded, in one alphabet.
fn is_base64(text: &str) -> bool {
    let body = text.trim_end_matches('=');
    let padding = text.len() - body.len();
    let standard = body
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/');
    let url_safe = body
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !standard && !url_safe {
        return false;
    }
    match padding {
        0 => body.len() % 4 != 1,
        1 => body.len() % 4 == 3,
        2 => body.len() % 4 == 2,
        _ => false,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`, with every field in range.
fn is_rfc3339(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<u32> {
        let slice = bytes.get(from..from + len)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    let at = |index: usize, expected: &[u8]| bytes.get(index).is_some_and(|b| expected.contains(b));
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits(0, 4),
        digits(5, 2),
        digits(8, 2),
        digits(11, 2),
        digits(14, 2),
        digits(17, 2),
    ) else {
        return false;
    };
    if !(at(4, b"-") && at(7, b"-") && at(10, b"Tt") && at(13, b":") && at(16, b":")) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    if !(1..=days_in_month).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    let mut index = 19;
    if at(index, b".") {
        let fraction = bytes[index + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if fraction == 0 {
            return false;
        }
        index += 1 + fraction;
    }
    match bytes.get(index) {
        Some(b'Z' | b'z') => index + 1 == bytes.len(),
        Some(b'+' | b'-') => {
            index + 6 == bytes.len()
                && at(index + 3, b":")
                && digits(index + 1, 2).is_some_and(|hours| hours <= 23)
                && digits(index + 4, 2).is_some_and(|minutes| minutes <= 59)
        }
        _ => false,
    }
}

/// The innermost message being read, and the `oneof` group when the reader is inside one.
fn enclosing(blocks: &[Block]) -> Option<(&String, Option<&String>)> {
    match blocks {
        [.., Block::Message(message), Block::Oneof(group)] => Some((message, Some(group))),
        [.., Block::Message(message)] => Some((message, None)),
        _ => None,
    }
}

/// One rpc header: `rpc Name(Request) returns ([stream ]Response) {`.
fn parse_rpc(line: &str, package: &str) -> Result<MethodSpec, String> {
    let malformed = || format!("`{line}` is not a one-line `rpc Name(Req) returns (Resp)` header");
    let rest = line.strip_prefix("rpc").ok_or_else(malformed)?.trim_start();
    let (name, rest) = rest.split_once('(').ok_or_else(malformed)?;
    let (request, rest) = rest.split_once(')').ok_or_else(malformed)?;
    let rest = rest
        .trim_start()
        .strip_prefix("returns")
        .ok_or_else(malformed)?
        .trim_start();
    let (response, _) = rest
        .strip_prefix('(')
        .and_then(|rest| rest.split_once(')'))
        .ok_or_else(malformed)?;
    let request = request.trim();
    if request.starts_with("stream ") {
        return Err(format!(
            "`{line}` streams its requests, which the JSON-RPC binding has no form for"
        ));
    }
    let (streaming, response) = match response.trim().strip_prefix("stream ") {
        Some(response) => (true, response.trim()),
        None => (false, response.trim()),
    };
    let qualify = |type_name: &str| {
        if type_name.contains('.') {
            type_name.to_string()
        } else {
            format!("{package}.{type_name}")
        }
    };
    let name = name.trim();
    if name.is_empty() || request.is_empty() || response.is_empty() {
        return Err(malformed());
    }
    Ok(MethodSpec {
        name: name.to_string(),
        params: qualify(request),
        result: qualify(response),
        streaming,
    })
}

/// `N` of an enum value line `NAME = N;` or `NAME = N [options];`.
fn enum_number(line: &str) -> Option<i64> {
    let (_, rest) = line.split_once('=')?;
    let number: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    number.parse().ok()
}

/// One field line: `[repeated|optional] TYPE name = N [options];`.
fn parse_field(line: &str, oneof: Option<String>) -> Result<Field, String> {
    if !line.ends_with(';') {
        return Err(format!(
            "`{line}` does not end in `;`; a field spanning lines is not supported by this reader"
        ));
    }
    let (declaration, _) = line
        .split_once('=')
        .ok_or_else(|| format!("`{line}` is not a field"))?;
    let declaration = declaration.trim();
    let (label, rest) = if let Some(rest) = declaration.strip_prefix("repeated ") {
        (Label::Repeated, rest.trim_start())
    } else if let Some(rest) = declaration.strip_prefix("optional ") {
        (Label::Optional, rest.trim_start())
    } else {
        (Label::Singular, declaration)
    };
    // A map type carries a space after its comma, so it ends at its `>`, not at whitespace.
    let (type_name, name) = if rest.starts_with("map<") {
        let end = rest
            .find('>')
            .ok_or_else(|| format!("`{line}` has an unclosed map type"))?;
        (rest[..=end].to_string(), rest[end + 1..].trim())
    } else {
        let (type_name, name) = rest
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("`{line}` names no field"))?;
        (type_name.to_string(), name.trim())
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("`{line}` has no valid field name"));
    }
    Ok(Field {
        proto_name: name.to_string(),
        json_name: json_name(name),
        label,
        type_name,
        required: line.contains("field_behavior) = REQUIRED"),
        oneof,
    })
}

/// The protobuf JSON name of a field: lowerCamelCase of the proto name, each `_` dropped and the
/// letter after it uppercased.
fn json_name(proto_name: &str) -> String {
    let mut out = String::with_capacity(proto_name.len());
    let mut upper_next = false;
    for c in proto_name.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.push(c.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A message or enum type name without its package: the proto refers to its own types unqualified.
fn short_type(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap_or(type_name)
}

/// `(key type, value type)` of a `map<K, V>` type, or `None` for any other type.
fn map_types(type_name: &str) -> Option<(&str, &str)> {
    let inner = type_name.strip_prefix("map<")?.strip_suffix('>')?;
    let (key, value) = inner.split_once(',')?;
    Some((key.trim(), value.trim()))
}

/// `NAME` of a `keyword NAME {` line.
fn block_name(words: &[&str], line_no: usize) -> Result<String, String> {
    words
        .get(1)
        .map(|name| name.trim_end_matches(['{', '}']).to_string())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("line {line_no}: a block with no name"))
}

/// The line up to a `//` that is not inside a string literal.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let bytes = line.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'/' if !in_string && bytes.get(index + 1) == Some(&b'/') => return &line[..index],
            _ => {}
        }
    }
    line
}

/// `{` and `}` outside string literals: option bodies such as `get: "/{id=tasks/*}"` carry
/// braces inside their strings.
fn count_braces(line: &str) -> (usize, usize) {
    let mut in_string = false;
    let (mut opens, mut closes) = (0, 0);
    for c in line.chars() {
        match c {
            '"' => in_string = !in_string,
            '{' if !in_string => opens += 1,
            '}' if !in_string => closes += 1,
            _ => {}
        }
    }
    (opens, closes)
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// `path`, or `<Message>` for the value being checked itself.
fn display(path: &str, message: &str) -> String {
    if path.is_empty() {
        format!("<{message}>")
    } else {
        path.to_string()
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// An envelope member as an error message shows it: its JSON, or `nothing` when absent.
fn shown(value: Option<&Value>) -> String {
    match value {
        None => "nothing".to_string(),
        Some(value) => value.to_string(),
    }
}
