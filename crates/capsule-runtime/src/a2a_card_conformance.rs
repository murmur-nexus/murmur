//! A strict protobuf-JSON check of an agent card against `lf.a2a.v1.AgentCard`, as the vendored
//! upstream `a2a.proto` defines it.
//!
//! The oracle is the proto itself rather than a hand-written mirror of it: the message blocks are
//! read out of the pinned file at test time, so the check follows the normative definition and
//! nothing else. A card is judged the way a strict protobuf JSON parser judges it — a key that is
//! not the JSON name of a field is an error, and so is a field the proto marks
//! `(google.api.field_behavior) = REQUIRED` that the card omits.
//!
//! This file uses only `std` and `serde_json` and no `crate::` path, because the `murmur-cli`
//! integration tests compile it too, through `#[path]` from `tests/common/mod.rs`. The
//! `include_str!` below resolves relative to this file, so it finds the proto from either crate.
//!
//! The reader covers the proto subset `a2a.proto` is written in: one field per line, top-level
//! `message`, `enum` and `service` blocks, `oneof` groups, `map<K, V>` fields, and nested `enum`
//! blocks. A nested `message` is reported as a checker error rather than guessed at.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// `a2a.proto` at tag `v1.0.0`, verbatim. See the README beside it for its provenance.
const A2A_PROTO: &str = include_str!("../tests/fixtures/a2a/v1.0.0/a2a.proto");

/// The message a served agent card is.
const AGENT_CARD_MESSAGE: &str = "AgentCard";

/// Every error a strict protobuf JSON parse of `card` as `lf.a2a.v1.AgentCard` would raise, each
/// naming the JSON path it is about (`capabilities.extensions[1].params`).
///
/// A proto the reader cannot follow is reported the same way, as an error, never as a panic.
pub(crate) fn check_agent_card(card: &Value) -> Result<(), Vec<String>> {
    let proto = Proto::parse(A2A_PROTO).map_err(|error| vec![format!("a2a.proto: {error}")])?;
    let mut errors = Vec::new();
    proto.check_message(card, AGENT_CARD_MESSAGE, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
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
    /// The `oneof` group the field belongs to; at most one member of a group may be set.
    oneof: Option<String>,
}

#[derive(Debug, Default)]
struct Proto {
    messages: HashMap<String, Vec<Field>>,
    /// Enum name → its value names.
    enums: HashMap<String, HashSet<String>>,
}

/// What a block the reader has entered is, so its closing brace ends the right thing.
enum Block {
    Message(String),
    Oneof(String),
    Enum(String),
}

impl Proto {
    fn parse(source: &str) -> Result<Proto, String> {
        let mut proto = Proto::default();
        let mut blocks: Vec<Block> = Vec::new();
        // Depth of a block whose contents are not read (`service`, `rpc`, option bodies): only its
        // braces are counted, until it closes.
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
                    Some("service" | "option") => skipped_depth = opens.saturating_sub(closes),
                    Some("syntax" | "package" | "import") => {}
                    _ => return Err(format!("line {line_no}: unexpected `{line}`")),
                },
                Some(Block::Enum(name)) => match words.first().copied() {
                    _ if line == "}" => {
                        blocks.pop();
                    }
                    Some("option" | "reserved") => skipped_depth = opens.saturating_sub(closes),
                    Some(value) if line.contains('=') => {
                        let name = name.clone();
                        proto
                            .enums
                            .entry(name)
                            .or_default()
                            .insert(value.to_string());
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
                Block::Message(name) | Block::Oneof(name) | Block::Enum(name) => name,
            };
            return Err(format!("block `{name}` is never closed"));
        }
        Ok(proto)
    }

    fn check_message(&self, value: &Value, message: &str, path: &str, errors: &mut Vec<String>) {
        let Some(fields) = self.messages.get(message) else {
            errors.push(format!(
                "`{}`: a2a.proto defines no message `{message}`",
                display(path)
            ));
            return;
        };
        let Some(object) = value.as_object() else {
            errors.push(format!(
                "`{}` must be a JSON object (message {message}), found {}",
                display(path),
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

        let mut oneofs_set: HashMap<&str, Vec<&str>> = HashMap::new();
        for field in fields {
            let field_path = join(path, &field.json_name);
            match object.get(&field.json_name) {
                None | Some(Value::Null) => {
                    if field.required {
                        errors.push(format!("missing required field `{field_path}`"));
                    }
                }
                Some(present) => {
                    if let Some(group) = &field.oneof {
                        oneofs_set
                            .entry(group.as_str())
                            .or_default()
                            .push(field.json_name.as_str());
                    }
                    self.check_field(present, field, &field_path, errors);
                }
            }
        }
        for (group, members) in oneofs_set {
            if members.len() > 1 {
                errors.push(format!(
                    "`{}` sets more than one member of oneof `{group}`: {}",
                    display(path),
                    members.join(", ")
                ));
            }
        }
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
            "string" | "bytes" => value.is_string(),
            "bool" => value.is_boolean(),
            "int32" | "int64" | "uint32" | "uint64" | "sint32" | "sint64" | "fixed32"
            | "fixed64" | "sfixed32" | "sfixed64" => match value {
                Value::Number(number) => number.is_i64() || number.is_u64(),
                Value::String(text) => text.parse::<i128>().is_ok(),
                _ => false,
            },
            "float" | "double" => match value {
                Value::Number(_) => true,
                Value::String(text) => {
                    matches!(text.as_str(), "NaN" | "Infinity" | "-Infinity")
                        || text.parse::<f64>().is_ok()
                }
                _ => false,
            },
            "google.protobuf.Struct" | "google.protobuf.Empty" => value.is_object(),
            "google.protobuf.Value" => true,
            "google.protobuf.Timestamp" => value.is_string(),
            _ => {
                let short = type_name.rsplit('.').next().unwrap_or(type_name);
                if let Some(values) = self.enums.get(short) {
                    match value {
                        Value::String(name) if values.contains(name) => {}
                        Value::String(name) => {
                            errors.push(format!("`{path}` is {name:?}, not a value of {short}"))
                        }
                        Value::Number(number) if number.is_i64() => {}
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
            errors.push(format!(
                "`{path}` must be a {type_name}, found {}",
                kind(value)
            ));
        }
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

fn display(path: &str) -> &str {
    if path.is_empty() {
        "<card>"
    } else {
        path
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
