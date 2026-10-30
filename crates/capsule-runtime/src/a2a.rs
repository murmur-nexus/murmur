use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use chrono::{DateTime, SecondsFormat, Utc};
use murmur_artifact::TaskAcceptance;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::{
    cancel::CancelSignal, identity::DOOR_EXTENSION_URI, lanes::TaskLane, origin::TaskProvenance,
};

// ── A2A protocol version ──────────────────────────────────────────────────────

/// The HTTP header an A2A request names its protocol version in. Matched case-insensitively.
pub const A2A_VERSION_HEADER: &str = "A2A-Version";

/// The one A2A protocol version the door speaks, and the value every murmur client sends in
/// [`A2A_VERSION_HEADER`].
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

/// Whether a request whose [`A2A_VERSION_HEADER`] lines carried `values`, in order, speaks a
/// version the door serves.
///
/// Exactly one line is accepted, holding [`A2A_PROTOCOL_VERSION`] or that version with a patch
/// number (`1.0.1`), surrounding whitespace aside: a patch number takes no part in negotiation.
/// No line, an empty value — which A2A reads as 0.3 — and more than one line are refused.
pub(crate) fn accepts_a2a_version(values: &[String]) -> bool {
    let [value] = values else {
        return false;
    };
    let value = value.trim();
    value == A2A_PROTOCOL_VERSION
        || value
            .strip_prefix(A2A_PROTOCOL_VERSION)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
}

/// The `VersionNotSupportedError` answering a request whose [`A2A_VERSION_HEADER`] lines carried
/// `values`. `requestedVersion` is the values as sent, joined with `, `, and `""` when there were
/// none.
pub(crate) fn version_not_supported(id: Value, values: &[String]) -> JsonRpcResponse {
    let requested = values
        .iter()
        .map(|value| value.trim())
        .collect::<Vec<_>>()
        .join(", ");
    JsonRpcResponse::a2a_error(
        id,
        A2aError::VersionNotSupported,
        &format!(
            "A2A version '{requested}' is not supported; this agent speaks {A2A_PROTOCOL_VERSION}"
        ),
        &[
            ("requestedVersion", requested.clone()),
            ("supportedVersions", A2A_PROTOCOL_VERSION.to_string()),
        ],
    )
}

// ── A2A extensions ────────────────────────────────────────────────────────────

/// The HTTP header a client activates A2A extensions in, and the header a response lists the
/// extensions it activated in. Matched case-insensitively.
pub const A2A_EXTENSIONS_HEADER: &str = "A2A-Extensions";

/// Whether the [`A2A_EXTENSIONS_HEADER`] lines a request carried, `values`, activate `uri`.
///
/// Each line is a comma-separated list: every entry is trimmed, empty entries are dropped, and
/// `uri` is activated when one entry equals it exactly. Any other URI is ignored, as an agent
/// ignores a requested extension it does not support.
pub(crate) fn activates_extension(values: &[String], uri: &str) -> bool {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry == uri)
}

/// The [`A2A_EXTENSIONS_HEADER`] line of a response that used `uris`, in that order, ending in
/// `\r\n`; empty when it used none.
pub(crate) fn extensions_echo(uris: &[&str]) -> String {
    if uris.is_empty() {
        return String::new();
    }
    format!("{A2A_EXTENSIONS_HEADER}: {}\r\n", uris.join(", "))
}

// ── JSON-RPC 2.0 envelope types ───────────────────────────────────────────────

/// `-32700`: the body is not JSON.
pub(crate) const PARSE_ERROR: i32 = -32700;
/// `-32600`: the body is JSON but not a JSON-RPC 2.0 request.
pub(crate) const INVALID_REQUEST: i32 = -32600;
/// `-32601`: the door serves no method of this name.
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
/// `-32602`: the method's parameters are not what it takes.
pub(crate) const INVALID_PARAMS: i32 = -32602;
/// `-32603`: the door failed to answer a request it accepted.
pub(crate) const INTERNAL_ERROR: i32 = -32603;

/// A JSON-RPC 2.0 request, as [`JsonRpcRequest::from_body`] accepts it. Its `jsonrpc` was
/// `"2.0"`, which is the only version there is to carry.
#[derive(Debug, Clone)]
pub(crate) struct JsonRpcRequest {
    /// A string or an integer.
    pub id: Value,
    pub method: String,
    /// The request's `params` as sent, or `{}` when it sent none. Not necessarily an object: a
    /// method refuses any other value with `-32602` itself.
    pub params: Value,
}

impl JsonRpcRequest {
    /// The request `body` carries, or the error that answers it.
    ///
    /// A body that is not JSON is `-32700` with `id: null`. A body that is JSON but not a JSON-RPC
    /// 2.0 request is `-32600`: one that is not an object (a batch among them), whose `jsonrpc` is
    /// not exactly `"2.0"`, whose `method` is not a string, or whose `id` is absent or neither a
    /// string nor an integer. That answer echoes the request's `id` when the `id` itself is valid,
    /// and is `null` otherwise. An omitted `params` reads as `{}`; members beyond the four are
    /// ignored.
    pub(crate) fn from_body(body: &str) -> Result<JsonRpcRequest, JsonRpcResponse> {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            return Err(JsonRpcResponse::err(
                Value::Null,
                PARSE_ERROR,
                "Invalid JSON payload",
            ));
        };
        let Value::Object(mut object) = value else {
            return Err(Self::invalid(Value::Null));
        };
        let id = object
            .remove("id")
            .filter(|id| id.is_string() || id.is_i64() || id.is_u64());
        let jsonrpc = object.remove("jsonrpc");
        let (Some(id), Some("2.0"), Some(Value::String(method))) = (
            id.clone(),
            jsonrpc.as_ref().and_then(Value::as_str),
            object.remove("method"),
        ) else {
            return Err(Self::invalid(id.unwrap_or(Value::Null)));
        };
        let params = object
            .remove("params")
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        Ok(JsonRpcRequest { id, method, params })
    }

    fn invalid(id: Value) -> JsonRpcResponse {
        JsonRpcResponse::err(id, INVALID_REQUEST, "Request payload validation error")
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Boxed so a response is small enough to travel in a `Result`'s error variant, which is how
    /// [`JsonRpcRequest::from_body`] answers a request it refuses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Box<JsonRpcError>>,
}

impl JsonRpcResponse {
    pub(crate) fn ok(id: Value, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    /// A JSON-RPC error with no `data`: the standard codes, `-32700` to `-32603`.
    pub(crate) fn err(id: Value, code: i32, message: &str) -> Self {
        Self::error(id, code, message, None)
    }

    /// The A2A error `error`, carrying a `google.rpc.ErrorInfo` in the `a2a-protocol.org` domain
    /// whose `metadata` is `metadata`.
    pub(crate) fn a2a_error(
        id: Value,
        error: A2aError,
        message: &str,
        metadata: &[(&str, String)],
    ) -> Self {
        let data = error_info(error.reason(), A2A_ERROR_DOMAIN, metadata);
        Self::error(id, error.code(), message, Some(vec![data]))
    }

    /// The murmur error `error`, carrying a `google.rpc.ErrorInfo` in the [`MURMUR_ERROR_DOMAIN`]
    /// whose `metadata` is `metadata`.
    pub(crate) fn murmur_error(
        id: Value,
        error: MurmurError,
        message: &str,
        metadata: &[(&str, String)],
    ) -> Self {
        let data = error_info(error.reason(), MURMUR_ERROR_DOMAIN, metadata);
        Self::error(id, error.code(), message, Some(vec![data]))
    }

    fn error(id: Value, code: i32, message: &str, data: Option<Vec<Value>>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(Box::new(JsonRpcError {
                code,
                message: message.to_string(),
                data,
            })),
        }
    }

    pub(crate) fn into_http_response(self) -> String {
        self.into_http_response_echoing(&[])
    }

    /// [`Self::into_http_response`], listing `extensions` in [`A2A_EXTENSIONS_HEADER`] when the
    /// response is a `result`. An error echoes nothing: a refused request used no extension.
    pub(crate) fn into_http_response_echoing(self, extensions: &[&str]) -> String {
        let echo = if self.error.is_none() {
            extensions_echo(extensions)
        } else {
            String::new()
        };
        let body = serde_json::to_string(&self).unwrap_or_else(|_| "{}".to_string());
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{echo}\r\n{}",
            body.len(),
            body,
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcError {
    pub code: i32,
    pub message: String,
    /// One `google.rpc.ErrorInfo` for an A2A or murmur error, and absent for a standard one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Vec<Value>>,
}

/// The `@type` of the one detail an A2A or murmur error's `data` carries.
const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The `ErrorInfo` `domain` of an A2A error.
pub(crate) const A2A_ERROR_DOMAIN: &str = "a2a-protocol.org";

/// The `ErrorInfo` `domain` of a [`MurmurError`].
pub(crate) const MURMUR_ERROR_DOMAIN: &str = "murmur.nexus";

/// A `google.rpc.ErrorInfo` detail. `metadata` holds string values only, as `ErrorInfo` requires.
fn error_info(reason: &str, domain: &str, metadata: &[(&str, String)]) -> Value {
    let metadata: serde_json::Map<String, Value> = metadata
        .iter()
        .map(|(key, value)| (key.to_string(), Value::String(value.clone())))
        .collect();
    serde_json::json!({
        "@type": ERROR_INFO_TYPE,
        "reason": reason,
        "domain": domain,
        "metadata": metadata,
    })
}

/// The A2A errors, A2A v1.0 §5.4, in code order.
///
/// The door answers six of them: [`Self::TaskNotFound`], [`Self::TaskNotCancelable`],
/// [`Self::PushNotificationNotSupported`], [`Self::UnsupportedOperation`],
/// [`Self::ContentTypeNotSupported`] and [`Self::VersionNotSupported`]. The others are here so a
/// client of a door names any error it is answered with through [`Self::from_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A2aError {
    TaskNotFound,
    TaskNotCancelable,
    PushNotificationNotSupported,
    UnsupportedOperation,
    ContentTypeNotSupported,
    InvalidAgentResponse,
    ExtendedAgentCardNotConfigured,
    ExtensionSupportRequired,
    VersionNotSupported,
}

impl A2aError {
    /// Every A2A error, in code order.
    pub const ALL: [A2aError; 9] = [
        A2aError::TaskNotFound,
        A2aError::TaskNotCancelable,
        A2aError::PushNotificationNotSupported,
        A2aError::UnsupportedOperation,
        A2aError::ContentTypeNotSupported,
        A2aError::InvalidAgentResponse,
        A2aError::ExtendedAgentCardNotConfigured,
        A2aError::ExtensionSupportRequired,
        A2aError::VersionNotSupported,
    ];

    /// The JSON-RPC `error.code`.
    pub fn code(self) -> i32 {
        match self {
            A2aError::TaskNotFound => -32001,
            A2aError::TaskNotCancelable => -32002,
            A2aError::PushNotificationNotSupported => -32003,
            A2aError::UnsupportedOperation => -32004,
            A2aError::ContentTypeNotSupported => -32005,
            A2aError::InvalidAgentResponse => -32006,
            A2aError::ExtendedAgentCardNotConfigured => -32007,
            A2aError::ExtensionSupportRequired => -32008,
            A2aError::VersionNotSupported => -32009,
        }
    }

    /// The `ErrorInfo` `reason`: the spec's error name in UPPER_SNAKE_CASE without `Error`.
    pub fn reason(self) -> &'static str {
        match self {
            A2aError::TaskNotFound => "TASK_NOT_FOUND",
            A2aError::TaskNotCancelable => "TASK_NOT_CANCELABLE",
            A2aError::PushNotificationNotSupported => "PUSH_NOTIFICATION_NOT_SUPPORTED",
            A2aError::UnsupportedOperation => "UNSUPPORTED_OPERATION",
            A2aError::ContentTypeNotSupported => "CONTENT_TYPE_NOT_SUPPORTED",
            A2aError::InvalidAgentResponse => "INVALID_AGENT_RESPONSE",
            A2aError::ExtendedAgentCardNotConfigured => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            A2aError::ExtensionSupportRequired => "EXTENSION_SUPPORT_REQUIRED",
            A2aError::VersionNotSupported => "VERSION_NOT_SUPPORTED",
        }
    }

    /// The A2A error whose code is `code`, or `None` for a code A2A does not assign.
    pub fn from_code(code: i32) -> Option<A2aError> {
        Self::ALL.into_iter().find(|error| error.code() == code)
    }
}

/// The errors a door answers a completion with: murmur's own protocol riding on `SendMessage`
/// with `x-murmur-task-origin: completion`, so none of them is an A2A error.
///
/// Their codes lie outside the range JSON-RPC 2.0 reserves, which leaves them to applications,
/// and the door extension documents them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MurmurError {
    /// The completion's `x-murmur-completion-session` is not the session running here.
    CompletionMisaddressed,
    /// The completion names no delegation, names one no task here waits for, or names one this
    /// session is ending.
    CompletionNotAwaited,
}

impl MurmurError {
    /// Every murmur error, in code order.
    pub const ALL: [MurmurError; 2] = [
        MurmurError::CompletionMisaddressed,
        MurmurError::CompletionNotAwaited,
    ];

    /// The JSON-RPC `error.code`.
    pub fn code(self) -> i32 {
        match self {
            MurmurError::CompletionMisaddressed => -31001,
            MurmurError::CompletionNotAwaited => -31002,
        }
    }

    /// The `ErrorInfo` `reason`.
    pub fn reason(self) -> &'static str {
        match self {
            MurmurError::CompletionMisaddressed => "COMPLETION_MISADDRESSED",
            MurmurError::CompletionNotAwaited => "COMPLETION_NOT_AWAITED",
        }
    }
}

// ── A2A protocol types ────────────────────────────────────────────────────────

/// The `mediaType` of every text part murmur builds.
pub(crate) const TEXT_MEDIA_TYPE: &str = "text/plain";

/// The `mediaType` of every data part murmur builds.
pub(crate) const DATA_MEDIA_TYPE: &str = "application/json";

/// Who sent a message, A2A v1.0 `Role`, spelled as its ProtoJSON enum name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Role {
    #[serde(rename = "ROLE_USER")]
    User,
    #[serde(rename = "ROLE_AGENT")]
    Agent,
}

impl Role {
    /// The role's ProtoJSON name, the one it serializes as.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::User => "ROLE_USER",
            Self::Agent => "ROLE_AGENT",
        }
    }
}

/// What one part carries: A2A v1.0 `Part`'s `content` oneof.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PartContent {
    Text(String),
    /// Base64 file bytes, as ProtoJSON spells `bytes`. The door refuses a message carrying one.
    Raw(String),
    /// A file by reference. The door refuses a message carrying one.
    Url(String),
    /// Any JSON value, `null` included.
    Data(Value),
}

impl PartContent {
    /// The content's field name, which is also the kind a `task_start` record lists.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Raw(_) => "raw",
            Self::Url(_) => "url",
            Self::Data(_) => "data",
        }
    }
}

/// The four field names of [`PartContent`], in the proto's order.
const PART_CONTENT_FIELDS: [&str; 4] = ["text", "raw", "url", "data"];

/// One A2A v1.0 `Part`.
///
/// Serialized as one object holding exactly one content field beside the optional `mediaType`,
/// `filename` and `metadata`. Read through [`Part::from_json`], which refuses an object that sets
/// no content field or more than one, so no part deserializes to nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Part {
    pub content: PartContent,
    pub media_type: Option<String>,
    pub filename: Option<String>,
    pub metadata: Option<Value>,
}

impl Part {
    /// A text part with `mediaType` [`TEXT_MEDIA_TYPE`].
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self::of(PartContent::Text(text.into()), TEXT_MEDIA_TYPE)
    }

    /// A data part with `mediaType` [`DATA_MEDIA_TYPE`].
    pub(crate) fn data(value: Value) -> Self {
        Self::of(PartContent::Data(value), DATA_MEDIA_TYPE)
    }

    fn of(content: PartContent, media_type: &str) -> Self {
        Self {
            content,
            media_type: Some(media_type.to_string()),
            filename: None,
            metadata: None,
        }
    }

    /// The part's text, when it is a text part.
    pub(crate) fn as_text(&self) -> Option<&str> {
        match &self.content {
            PartContent::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The part's value, when it is a data part.
    #[cfg(test)]
    pub(crate) fn as_data(&self) -> Option<&Value> {
        match &self.content {
            PartContent::Data(value) => Some(value),
            _ => None,
        }
    }

    /// The part as ProtoJSON.
    pub(crate) fn to_json(&self) -> Value {
        let mut object = serde_json::Map::new();
        let (field, value) = match &self.content {
            PartContent::Text(text) => ("text", Value::String(text.clone())),
            PartContent::Raw(raw) => ("raw", Value::String(raw.clone())),
            PartContent::Url(url) => ("url", Value::String(url.clone())),
            PartContent::Data(value) => ("data", value.clone()),
        };
        object.insert(field.to_string(), value);
        if let Some(media_type) = &self.media_type {
            object.insert("mediaType".to_string(), Value::String(media_type.clone()));
        }
        if let Some(filename) = &self.filename {
            object.insert("filename".to_string(), Value::String(filename.clone()));
        }
        if let Some(metadata) = &self.metadata {
            object.insert("metadata".to_string(), metadata.clone());
        }
        Value::Object(object)
    }

    /// Why `value` sets no part content or more than one, or `None` when it sets exactly one.
    /// A present `null` counts as set, as ProtoJSON reads it.
    fn content_count_error(value: &Value) -> Option<String> {
        let Some(object) = value.as_object() else {
            return Some("is not an object".to_string());
        };
        let set: Vec<&str> = PART_CONTENT_FIELDS
            .into_iter()
            .filter(|field| object.contains_key(*field))
            .collect();
        match set.as_slice() {
            [_] => None,
            [] => Some("sets none of text, raw, url and data".to_string()),
            more => Some(format!(
                "sets {}; a part sets exactly one of text, raw, url and data",
                more.join(" and ")
            )),
        }
    }

    /// The part `value` is, or why it is not one: an object setting exactly one of `text`, `raw`,
    /// `url` and `data`, whose `text`, `raw`, `url`, `mediaType` and `filename` are strings where
    /// present. Unknown fields are ignored.
    pub(crate) fn from_json(value: &Value) -> Result<Part, String> {
        if let Some(error) = Self::content_count_error(value) {
            return Err(error);
        }
        let object = value.as_object().expect("checked to be an object");
        let string = |field: &str| -> Result<Option<String>, String> {
            match object.get(field) {
                None => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.clone())),
                Some(_) => Err(format!("{field} is not a string")),
            }
        };
        let content = if let Some(text) = string("text")? {
            PartContent::Text(text)
        } else if let Some(raw) = string("raw")? {
            PartContent::Raw(raw)
        } else if let Some(url) = string("url")? {
            PartContent::Url(url)
        } else {
            PartContent::Data(object.get("data").cloned().unwrap_or(Value::Null))
        };
        Ok(Part {
            content,
            media_type: string("mediaType")?,
            filename: string("filename")?,
            metadata: object.get("metadata").cloned(),
        })
    }
}

impl Serialize for Part {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Part::from_json(&value).map_err(|error| serde::de::Error::custom(format!("part {error}")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2aTask {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<A2aArtifact>>,
    /// `{"murmur": {...}}` for a terminal task with something to say: `noAnswer: true` for one
    /// ended through `end-without-answer`, `noAnswerBelow`, the members further down that gave it
    /// none, and `runningCommands`, the background shell commands still running when it ended.
    /// Absent when it has none of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl A2aTask {
    /// The artifact whose `artifactId` is `artifact_id`.
    pub(crate) fn artifact(&self, artifact_id: &str) -> Option<&A2aArtifact> {
        self.artifacts
            .as_deref()?
            .iter()
            .find(|artifact| artifact.artifact_id == artifact_id)
    }

    /// The text of the status message, when the status carries one with a text part.
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status.message.as_ref()?.text()
    }
}

/// An A2A v1.0 `Artifact`. Murmur names each artifact it builds once per task, so the name is
/// also its `artifactId`: unique within the task, and the same on every read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2aArtifact {
    pub artifact_id: String,
    #[serde(default)]
    pub name: String,
    pub parts: Vec<Part>,
}

impl A2aArtifact {
    /// The artifact `name`, holding `parts`.
    pub(crate) fn named(name: &str, parts: Vec<Part>) -> Self {
        Self {
            artifact_id: name.to_string(),
            name: name.to_string(),
            parts,
        }
    }

    /// The text of the first part, when it is a text part.
    pub(crate) fn first_text(&self) -> Option<&str> {
        self.parts.first()?.as_text()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskStatus {
    pub state: TaskState,
    /// What the task's final status said, for a terminal task that said something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StatusMessage>,
    /// When this status was recorded, as [`status_timestamp`] writes it. Every status murmur
    /// writes carries one; a peer's answer may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

impl TaskStatus {
    /// A status in `state` with no message, recorded now.
    pub(crate) fn of(state: TaskState) -> Self {
        Self::at(state, None, status_now())
    }

    /// A status in `state` saying `message`, recorded at `when`.
    pub(crate) fn at(
        state: TaskState,
        message: Option<StatusMessage>,
        when: DateTime<Utc>,
    ) -> Self {
        Self {
            state,
            message,
            timestamp: Some(status_timestamp(when)),
        }
    }
}

/// The current instant, truncated to the millisecond a status timestamp carries, so the string
/// [`status_timestamp`] writes parses back to exactly the instant stored.
pub(crate) fn status_now() -> DateTime<Utc> {
    let now = Utc::now();
    DateTime::from_timestamp_millis(now.timestamp_millis()).unwrap_or(now)
}

/// The instant of serialized `status`'s `timestamp`, asserting it is one [`status_timestamp`]
/// could have written: RFC 3339, in UTC with a `Z`, to the millisecond.
#[cfg(test)]
pub(crate) fn assert_status_timestamp(status: &Value) -> DateTime<Utc> {
    let timestamp = status["timestamp"]
        .as_str()
        .unwrap_or_else(|| panic!("no timestamp: {status}"));
    let parsed = DateTime::parse_from_rfc3339(timestamp)
        .unwrap_or_else(|error| panic!("{timestamp} is not RFC 3339: {error}"))
        .with_timezone(&Utc);
    assert_eq!(status_timestamp(parsed), timestamp, "{status}");
    parsed
}

/// Asserts `status` is a serialized status in `state` with no message and a `timestamp`
/// [`status_timestamp`] could have written.
#[cfg(test)]
pub(crate) fn assert_bare_status(status: &Value, state: &str) {
    assert_status_timestamp(status);
    assert_eq!(
        *status,
        serde_json::json!({"state": state, "timestamp": status["timestamp"]})
    );
}

/// `at` as a status's `timestamp`: RFC 3339 in UTC with milliseconds and a `Z`, such as
/// `2026-10-13T09:00:00.123Z`, which is what A2A v1.0's `google.protobuf.Timestamp` takes.
pub(crate) fn status_timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A task status's message: an A2A message from the agent about one task, with one text part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusMessage {
    pub message_id: String,
    pub context_id: String,
    pub task_id: String,
    pub role: Role,
    pub parts: Vec<Part>,
}

impl StatusMessage {
    /// The agent's message `text` about `task_id`, in `context_id`.
    pub(crate) fn agent(task_id: &str, context_id: &str, text: &str) -> Self {
        Self {
            message_id: format!("msg_{task_id}_status"),
            context_id: context_id.to_string(),
            task_id: task_id.to_string(),
            role: Role::Agent,
            parts: vec![Part::text(text)],
        }
    }

    /// The text of the first part, when it is a text part.
    pub(crate) fn text(&self) -> Option<&str> {
        self.parts.first()?.as_text()
    }
}

/// How a finished task ended, as its final status reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskEnding {
    /// The final status's message.
    message: String,
    /// The task's response text, kept only for a `completed` task.
    response: Option<String>,
    /// Whether the task ended through `end-without-answer`.
    no_answer: bool,
    /// `(member, status)` for each member further down that gave the task no answer.
    no_answer_below: Vec<(String, String)>,
    /// The background shell commands still running when the task ended, oldest first.
    running_commands: Vec<RunningCommand>,
}

/// A background shell command still running when a task ended, as the task's
/// `metadata.murmur.runningCommands` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningCommand {
    pub work_id: String,
    pub binary: String,
    /// The command text the model wrote, without the binary name.
    pub command: String,
    /// From spawn to the task's end.
    pub running_ms: u64,
}

/// The most characters of a running command's text the metadata carries, before the `…`.
const RUNNING_COMMAND_TEXT_CHARS: usize = 200;

impl TaskEnding {
    /// The `metadata` `GetTask` carries for this ending, or `None` when it says nothing.
    fn metadata(&self) -> Option<serde_json::Value> {
        let mut murmur = serde_json::Map::new();
        if self.no_answer {
            murmur.insert("noAnswer".to_string(), serde_json::Value::Bool(true));
        }
        if !self.no_answer_below.is_empty() {
            let below = self
                .no_answer_below
                .iter()
                .map(|(member, status)| serde_json::json!({"member": member, "status": status}))
                .collect();
            murmur.insert("noAnswerBelow".to_string(), serde_json::Value::Array(below));
        }
        if !self.running_commands.is_empty() {
            let running = self
                .running_commands
                .iter()
                .map(|running| {
                    let mut command: String = running
                        .command
                        .chars()
                        .take(RUNNING_COMMAND_TEXT_CHARS)
                        .collect();
                    if running
                        .command
                        .chars()
                        .nth(RUNNING_COMMAND_TEXT_CHARS)
                        .is_some()
                    {
                        command.push('…');
                    }
                    serde_json::json!({
                        "workId": running.work_id,
                        "binary": running.binary,
                        "command": command,
                        "runningMs": running.running_ms,
                    })
                })
                .collect();
            murmur.insert(
                "runningCommands".to_string(),
                serde_json::Value::Array(running),
            );
        }
        (!murmur.is_empty()).then(|| serde_json::json!({ "murmur": murmur }))
    }
}

/// The name and `artifactId` of the artifact a completed task's response is carried in over
/// `GetTask`.
pub(crate) const RESPONSE_ARTIFACT: &str = "response";

/// The name and `artifactId` of the artifact an `input-required` task's prompt is carried in.
pub(crate) const PROMPT_ARTIFACT: &str = "prompt";

/// A task's state.
///
/// It has two spellings. On the A2A wire it is the v1.0 ProtoJSON enum name, which serde and
/// [`Self::wire_name`] give and which is the only spelling it deserializes from; every stream
/// event and murmur frame carries that `TASK_STATE_*` name too. Everywhere murmur chooses its own
/// words — every sentence written for the model, the guest's `task-result.state`, `mur` output
/// and trace records — it is murmur's word, [`Self::as_str`].
///
/// The door never produces `TASK_STATE_AUTH_REQUIRED`: it authenticates a request before any task
/// exists, so no task can wait for authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    /// A person stopped this task. Terminal, and distinct from `Failed`: nothing went wrong, the
    /// work was called off.
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
}

impl TaskState {
    /// Every state, in the proto's order.
    pub(crate) const ALL: [TaskState; 7] = [
        TaskState::Submitted,
        TaskState::Working,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
        TaskState::InputRequired,
        TaskState::Rejected,
    ];

    /// The state's A2A v1.0 ProtoJSON name, the one it serializes as.
    pub(crate) fn wire_name(&self) -> &'static str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Rejected => "TASK_STATE_REJECTED",
            Self::Canceled => "TASK_STATE_CANCELED",
        }
    }

    /// The state whose A2A v1.0 ProtoJSON name is `wire`, or `None` for `TASK_STATE_UNSPECIFIED`,
    /// `TASK_STATE_AUTH_REQUIRED`, which no task here is ever in, and anything else.
    pub(crate) fn from_wire_name(wire: &str) -> Option<TaskState> {
        Self::ALL
            .into_iter()
            .find(|state| state.wire_name() == wire)
    }

    /// The state in murmur's own word, the one sentences written for the model carry:
    /// [`TaskRegistry::reference_block`] and `call-member`'s answers. The guest's
    /// `task-result.state` and `mur` output reach the same word from a wire name through
    /// [`murmur_state_name`], and a test holds the two equal.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::InputRequired => "input-required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Canceled => "canceled",
        }
    }

    /// Whether no further work will be done on a task in this state.
    ///
    /// The one rule a cancel reads: a terminal task is left exactly as it is, and only a live one
    /// can be stopped.
    pub(crate) fn is_terminal(&self) -> bool {
        match self {
            Self::Submitted | Self::Working | Self::InputRequired => false,
            Self::Completed | Self::Failed | Self::Rejected | Self::Canceled => true,
        }
    }
}

/// Murmur's word for the A2A v1.0 task state `wire`, or `None` for `TASK_STATE_UNSPECIFIED` and
/// anything that is not a v1.0 state name.
///
/// Total over the eight meaningful v1.0 states, `TASK_STATE_AUTH_REQUIRED` among them, which is
/// `auth-required`, A2A 0.3's word for it: a peer that is not murmur may answer it, though this
/// door never does. This is how a state read off the wire reaches the guest and `mur` output.
pub fn murmur_state_name(wire: &str) -> Option<&'static str> {
    Some(match wire {
        "TASK_STATE_SUBMITTED" => "submitted",
        "TASK_STATE_WORKING" => "working",
        "TASK_STATE_COMPLETED" => "completed",
        "TASK_STATE_FAILED" => "failed",
        "TASK_STATE_CANCELED" => "canceled",
        "TASK_STATE_INPUT_REQUIRED" => "input-required",
        "TASK_STATE_REJECTED" => "rejected",
        "TASK_STATE_AUTH_REQUIRED" => "auth-required",
        _ => return None,
    })
}

// ── Incoming messages ─────────────────────────────────────────────────────────

/// The most distinct task ids one message may name in `referenceTaskIds`.
pub(crate) const MAX_REFERENCE_TASK_IDS: usize = 16;

/// A `SendMessage` or `SendStreamingMessage` message the door read in full, as
/// [`read_send_params`] returns it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IncomingMessage {
    pub message_id: String,
    pub context_id: Option<String>,
    /// The task this message continues. An empty `taskId` reads as `None`.
    pub task_id: Option<String>,
    /// `referenceTaskIds` with duplicates collapsed to their first occurrence.
    pub reference_task_ids: Vec<String>,
    /// Text and data parts only: a message carrying any other part is refused.
    pub parts: Vec<Part>,
    /// Whether `metadata` carries the door extension's key, [`DOOR_EXTENSION_URI`]: the message
    /// used that extension, so a response that serves it lists the URI in
    /// [`A2A_EXTENSIONS_HEADER`].
    pub door_extension: bool,
    /// `metadata[DOOR_EXTENSION_URI].forgetSession`: the task this message starts drops the
    /// harness session its context names before its first attempt. Never `true` on a message
    /// that names a `taskId`.
    pub forget_session: bool,
    /// `configuration.returnImmediately`: `SendMessage` answers once the task is accepted rather
    /// than once it settles. `SendStreamingMessage` reads no meaning into it.
    pub return_immediately: bool,
}

/// Why the door refused a message, and so answered no task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MessageRefusal {
    /// `-32602`: the message is not an A2A v1.0 message. The text says what is wrong.
    InvalidParams(String),
    /// `-32005` `ContentTypeNotSupportedError`: the part at `part_index` is a file part, `raw` or
    /// `url`, which this door does not read.
    ContentTypeNotSupported {
        part_index: usize,
        kind: &'static str,
        media_type: Option<String>,
    },
    /// `-32003` `PushNotificationNotSupportedError`: `configuration.taskPushNotificationConfig`
    /// asks to be notified, and this agent sends no push notifications.
    PushNotificationNotSupported,
}

impl MessageRefusal {
    /// The JSON-RPC error answering request `id` with this refusal.
    pub(crate) fn into_response(self, id: Value) -> JsonRpcResponse {
        match self {
            Self::InvalidParams(message) => {
                JsonRpcResponse::err(id, INVALID_PARAMS, &format!("Invalid params: {message}"))
            }
            Self::ContentTypeNotSupported {
                part_index,
                kind,
                media_type,
            } => {
                let mut metadata = vec![("partIndex", part_index.to_string())];
                if let Some(media_type) = &media_type {
                    metadata.push(("mediaType", media_type.clone()));
                }
                JsonRpcResponse::a2a_error(
                    id,
                    A2aError::ContentTypeNotSupported,
                    &format!(
                        "message.parts[{part_index}] is a {kind} part; this agent reads text and \
                         data parts only"
                    ),
                    &metadata,
                )
            }
            Self::PushNotificationNotSupported => push_notification_not_supported(id),
        }
    }
}

/// The `PushNotificationNotSupportedError` answering request `id`: this agent's card says
/// `capabilities.pushNotifications: false`, so it neither stores a push config nor notifies.
pub(crate) fn push_notification_not_supported(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::a2a_error(
        id,
        A2aError::PushNotificationNotSupported,
        "push notifications are not supported by this agent",
        &[],
    )
}

/// The message a `SendMessage` or `SendStreamingMessage` request's `params` carries, or why it is
/// refused.
///
/// Read by hand rather than by serde, in this order, each failure `-32602` but the last:
/// `params.message` is an object; `messageId` is a non-empty string; `role` is `ROLE_USER`; `parts`
/// is a non-empty array; every part sets exactly one of `text`, `raw`, `url` and `data`; `text`,
/// `raw`, `url`, `mediaType` and `filename` are strings where present; `taskId` and `contextId`
/// are strings where present; `referenceTaskIds` is an array of non-empty strings naming at most
/// [`MAX_REFERENCE_TASK_IDS`] distinct tasks; `metadata[DOOR_EXTENSION_URI]` is an object where
/// present; its `forgetSession` is a boolean where present; `forgetSession: true` names no
/// `taskId`; `configuration.returnImmediately` is a boolean where present and not `null`. Then the
/// first `raw` or `url` part is [`MessageRefusal::ContentTypeNotSupported`], and a
/// `configuration.taskPushNotificationConfig` that is present and not `null` is
/// [`MessageRefusal::PushNotificationNotSupported`]. The rest of `configuration` and unknown
/// fields are ignored, a `configuration` that is not an object, a `metadata` that is not an object
/// and every other key of it and of the door extension's object among them.
pub(crate) fn read_send_params(params: &Value) -> Result<IncomingMessage, MessageRefusal> {
    let invalid = |message: String| Err(MessageRefusal::InvalidParams(message));
    let Some(message) = params.get("message").and_then(Value::as_object) else {
        return invalid("params.message must be an A2A Message object".to_string());
    };
    let message_id = match message.get("messageId") {
        Some(Value::String(id)) if !id.is_empty() => id.clone(),
        _ => return invalid("message.messageId must be a non-empty string".to_string()),
    };
    if message.get("role").and_then(Value::as_str) != Some(Role::User.wire_name()) {
        return invalid(format!(
            "message.role must be {}: a message to this agent is from the user",
            Role::User.wire_name()
        ));
    }
    let raw_parts = match message.get("parts") {
        Some(Value::Array(parts)) if !parts.is_empty() => parts,
        _ => return invalid("message.parts must be a non-empty array".to_string()),
    };
    for (index, part) in raw_parts.iter().enumerate() {
        if let Some(error) = Part::content_count_error(part) {
            return invalid(format!("message.parts[{index}] {error}"));
        }
    }
    let mut parts = Vec::with_capacity(raw_parts.len());
    for (index, part) in raw_parts.iter().enumerate() {
        match Part::from_json(part) {
            Ok(part) => parts.push(part),
            Err(error) => return invalid(format!("message.parts[{index}].{error}")),
        }
    }
    let optional_string = |field: &str| -> Result<Option<String>, MessageRefusal> {
        match message.get(field) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(MessageRefusal::InvalidParams(format!(
                "message.{field} must be a string"
            ))),
        }
    };
    let task_id = optional_string("taskId")?.filter(|id| !id.is_empty());
    let context_id = optional_string("contextId")?;
    let reference_task_ids = match message.get("referenceTaskIds") {
        None => Vec::new(),
        Some(Value::Array(ids)) => {
            let mut distinct: Vec<String> = Vec::new();
            for id in ids {
                match id {
                    Value::String(id) if !id.is_empty() => {
                        if !distinct.contains(id) {
                            distinct.push(id.clone());
                        }
                    }
                    _ => {
                        return invalid(
                            "message.referenceTaskIds must hold non-empty strings".to_string(),
                        )
                    }
                }
            }
            if distinct.len() > MAX_REFERENCE_TASK_IDS {
                return invalid(format!(
                    "message.referenceTaskIds names {} tasks; at most {MAX_REFERENCE_TASK_IDS} \
                     may be referenced",
                    distinct.len()
                ));
            }
            distinct
        }
        Some(_) => return invalid("message.referenceTaskIds must be an array".to_string()),
    };
    let door_metadata = message
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(DOOR_EXTENSION_URI));
    let forget_session = match door_metadata {
        None => false,
        Some(Value::Object(door)) => match door.get("forgetSession") {
            None => false,
            Some(Value::Bool(forget)) => *forget,
            Some(_) => {
                return invalid(format!(
                    "message.metadata[\"{DOOR_EXTENSION_URI}\"].forgetSession must be a boolean"
                ))
            }
        },
        Some(_) => {
            return invalid(format!(
                "message.metadata[\"{DOOR_EXTENSION_URI}\"] must be an object"
            ))
        }
    };
    if let (true, Some(task_id)) = (forget_session, &task_id) {
        return invalid(format!(
            "forgetSession starts a new conversation, so it goes on a message that starts a task, \
             not on one that continues task {task_id}"
        ));
    }
    let configuration = params.get("configuration");
    let return_immediately = match configuration.and_then(|c| c.get("returnImmediately")) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(immediately)) => *immediately,
        Some(_) => return invalid("configuration.returnImmediately must be a boolean".to_string()),
    };
    if let Some((part_index, part)) = parts
        .iter()
        .enumerate()
        .find(|(_, part)| matches!(part.content, PartContent::Raw(_) | PartContent::Url(_)))
    {
        return Err(MessageRefusal::ContentTypeNotSupported {
            part_index,
            kind: part.content.kind(),
            media_type: part.media_type.clone(),
        });
    }
    if configuration
        .and_then(|configuration| configuration.get("taskPushNotificationConfig"))
        .is_some_and(|config| !config.is_null())
    {
        return Err(MessageRefusal::PushNotificationNotSupported);
    }
    Ok(IncomingMessage {
        message_id,
        context_id,
        task_id,
        reference_task_ids,
        parts,
        door_extension: door_metadata.is_some(),
        forget_session,
        return_immediately,
    })
}

impl IncomingMessage {
    /// The message as the agent reads it: each part in order, joined by `"\n"`. A text part is its
    /// text; a data part is its value pretty-printed in a code fence labelled `data`, with the
    /// part's `mediaType` and `filename` when it has them. No part's `metadata` is rendered.
    pub(crate) fn agent_text(&self) -> String {
        self.parts
            .iter()
            .map(|part| match &part.content {
                PartContent::Text(text) => text.clone(),
                PartContent::Data(value) => {
                    let mut label = "data".to_string();
                    if let Some(media_type) = &part.media_type {
                        label.push_str(&format!(" media-type={}", fence_label_value(media_type)));
                    }
                    if let Some(filename) = &part.filename {
                        label.push_str(&format!(" filename={}", fence_label_value(filename)));
                    }
                    let body = serde_json::to_string_pretty(value).unwrap_or_default();
                    code_fence(&label, &body)
                }
                PartContent::Raw(_) | PartContent::Url(_) => {
                    unreachable!("read_send_params refuses every file part")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The kind of each part, in order: `text` or `data`.
    pub(crate) fn part_kinds(&self) -> Vec<&'static str> {
        self.parts.iter().map(|part| part.content.kind()).collect()
    }
}

/// `body` in a Markdown code fence whose opening line is the fence followed by `label`. The fence
/// is a run of backticks one longer than the longest run in `body`, and at least three, so nothing
/// in `body` can close it.
pub(crate) fn code_fence(label: &str, body: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{label}\n{body}\n{fence}")
}

/// `value` as one token of a fence's opening line: whitespace, backticks and `=` become `_`.
fn fence_label_value(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '`' || c == '=' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

// ── Client helpers ────────────────────────────────────────────────────────────

/// The A2A v1.0 message a murmur client sends a door: from `ROLE_USER`, with one text part.
/// `contextId` is written only when given.
pub(crate) fn user_message(message_id: &str, context_id: Option<&str>, text: &str) -> Value {
    let mut message = serde_json::json!({
        "messageId": message_id,
        "role": Role::User.wire_name(),
        "parts": [Part::text(text).to_json()],
    });
    if let Some(context_id) = context_id {
        message["contextId"] = Value::String(context_id.to_string());
    }
    message
}

/// The `SendMessage` params a murmur client sends a door: `message` with
/// `configuration.returnImmediately: true`, so the door answers once the task is accepted rather
/// than once it settles.
pub(crate) fn send_message_params(message: Value) -> Value {
    serde_json::json!({
        "message": message,
        "configuration": { "returnImmediately": true },
    })
}

/// A `SendMessageResponse`, as a door's `SendMessage` `result` carries it: the oneof of a task and
/// a message.
#[derive(Debug, Clone)]
pub(crate) enum SendMessageResult {
    Task(Box<A2aTask>),
    /// The message, as sent. Murmur reads nothing in it.
    Message(Value),
}

impl SendMessageResult {
    /// The member `result` sets, or why it is not a `SendMessageResponse`: it sets both members or
    /// neither, or its `task` is not a v1.0 task.
    pub(crate) fn from_result(result: &Value) -> Result<SendMessageResult, String> {
        match send_message_member(result)? {
            ("task", task) => serde_json::from_value(task.clone())
                .map(|task| SendMessageResult::Task(Box::new(task)))
                .map_err(|error| format!("its task is not an A2A task: {error}")),
            (_, message) => Ok(SendMessageResult::Message(message.clone())),
        }
    }
}

/// Which member of the `SendMessageResponse` oneof `result` sets, `"task"` or `"message"`, with
/// its value, or why it sets neither or both.
pub(crate) fn send_message_member(result: &Value) -> Result<(&'static str, &Value), String> {
    match (result.get("task"), result.get("message")) {
        (Some(task), None) => Ok(("task", task)),
        (None, Some(message)) => Ok(("message", message)),
        (Some(_), Some(_)) => Err("it answered both a task and a message".to_string()),
        (None, None) => Err(format!(
            "it answered neither a task nor a message: {result}"
        )),
    }
}

/// What [`TaskRegistry::request_cancel`] did.
///
/// Three outcomes and only one of them is an error at the door: cancelling a task that has
/// already ended is a clean no-op, because the caller's intent — "do no more work on this" — is
/// already true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// The task was live and is now `Canceled`. Its [`CancelSignal`] has been raised.
    Accepted,
    /// The task had already reached a terminal state, which is left untouched.
    AlreadyTerminal,
    /// No task with this id was ever enqueued here.
    Unknown,
}

// ── Task slot state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) enum TaskSlotState {
    Empty,
    Running {
        task_id: String,
        context_id: String,
        /// The lane the queue chose this task out of. Set and cleared with the rest of the
        /// variant, so it cannot name a lane no task is running in.
        lane: TaskLane,
    },
    /// The last task that ran has ended.
    Done,
}

// ── TaskRegistry — multi-task history tracker ─────────────────────────────────

/// Replaces the bare `Arc<Mutex<TaskSlotState>>` at the serve_http boundary.
/// Tracks all tasks (active + historical) for queue-mode capsules.
pub(crate) struct TaskRegistry {
    pub(crate) active_slot: TaskSlotState,
    /// All tasks ever enqueued: task_id → (state, context_id)
    pub(crate) history: HashMap<String, (TaskState, String)>,
    pub(crate) pending_count: usize,
    pub(crate) queue_depth: usize,
    pub(crate) task_acceptance: TaskAcceptance,
    /// Pending input waiters: task_id → (prompt, oneshot sender)
    input_waiters: HashMap<String, (String, oneshot::Sender<String>)>,
    /// Tasks whose `request-input` wait timed out, until the agent loop takes the mark.
    input_timeouts: HashSet<String>,
    /// One signal per task anybody has asked about, whether or not it has been cancelled.
    ///
    /// Minted on demand rather than at enqueue, because both ends need one before the task is
    /// running: the task loop watches a task it is about to activate, and the door cancels one
    /// that is still queued.
    cancels: HashMap<String, CancelSignal>,
    /// Which completed turn the capsule's exported files are as of, shared with the resource
    /// plane. Lives here because every terminal state passes through this registry, so a third
    /// [`Self::finish_task`] call site cannot appear with no matching increment beside it.
    resource_generation: Arc<AtomicU64>,
    /// Set by [`Self::close_to_new_work`] once the task loop has ended. A closed registry
    /// accepts nothing, whatever its acceptance mode and depth.
    closed: bool,
    /// How each finished task ended, recorded by [`Self::record_ending`].
    endings: HashMap<String, TaskEnding>,
    /// The door credential that started each task a credential other than the operator's
    /// started: a declared credential's name, or `member:<from>` for a formation token.
    owners: HashMap<String, String>,
    /// The last revision handed out. Every change to a task's state or ending takes the next one.
    revision: u64,
    /// Each task's revision as of its last change, which orders `ListTasks`, newest first.
    revisions: HashMap<String, u64>,
    /// The instant of each task's last change, stamped beside its revision: its status's
    /// `timestamp`, and what `ListTasks`' `statusTimestampAfter` compares against.
    status_times: HashMap<String, DateTime<Utc>>,
    /// Tasks kept working under `lifecycle.keep_working` that hold an answer from an earlier turn
    /// end, which the session deadline ends `completed` with that answer rather than `canceled`.
    standing_answers: HashSet<String>,
    /// Tasks holding a standing answer whose signal the session deadline raised without
    /// recording `Canceled`; read by [`Self::stopped_at_deadline`].
    stopped_at_deadline: HashSet<String>,
    /// Tasks the session deadline's cancel was accepted for; read by
    /// [`Self::canceled_at_deadline`].
    canceled_at_deadline: HashSet<String>,
}

/// The `status.message` of a task refused because the session ended before it started.
pub(crate) const REJECTED_SESSION_ENDED_MESSAGE: &str =
    "task rejected: the session ended before this task started";

/// The `status.message` of a task refused because `mur stop` ended the session before it started.
pub(crate) const REJECTED_SESSION_STOPPED_MESSAGE: &str =
    "task rejected: the session was stopped before this task started";

/// The `status.message` the door's `SendStreamingMessage` refusal carries once the registry is closed.
pub(crate) const REJECTED_SESSION_CLOSING_MESSAGE: &str = "task rejected: the session is closing";

/// The `status.message` of a task refused because the capsule has no room for it. A `call-member`
/// call reads exactly this text as "busy" and offers the task again; every other refusal ends it.
pub(crate) const REJECTED_BUSY_MESSAGE: &str = "task rejected: capsule is busy";

impl TaskRegistry {
    pub(crate) fn new(queue_depth: usize, task_acceptance: TaskAcceptance) -> Self {
        Self {
            active_slot: TaskSlotState::Empty,
            history: HashMap::new(),
            pending_count: 0,
            queue_depth,
            task_acceptance,
            input_waiters: HashMap::new(),
            input_timeouts: HashSet::new(),
            cancels: HashMap::new(),
            resource_generation: Arc::new(AtomicU64::new(0)),
            closed: false,
            endings: HashMap::new(),
            owners: HashMap::new(),
            revision: 0,
            revisions: HashMap::new(),
            status_times: HashMap::new(),
            standing_answers: HashSet::new(),
            stopped_at_deadline: HashSet::new(),
            canceled_at_deadline: HashSet::new(),
        }
    }

    /// A handle on the generation counter, for a reader that must not take this registry's lock.
    ///
    /// The resource plane serves reads while a turn is running, so it holds its own `Arc` and
    /// loads it atomically. Taking the registry mutex to answer a `GET` would make a read wait on
    /// the agent loop, which is the one thing this plane promises never to do.
    pub(crate) fn resource_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.resource_generation)
    }

    /// Advance the generation by one. Called at each [`Self::finish_task`] call site, immediately
    /// after the task reaches a terminal state.
    ///
    /// Provenance, not a pin: it answers "these bytes are as of turn N" and nothing else. No
    /// request selects a generation, no response is refused because it moved, and no superseded
    /// bytes are retained.
    pub(crate) fn advance_resource_generation(&self) {
        self.resource_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn can_accept(&self) -> bool {
        if self.closed {
            return false;
        }
        match self.task_acceptance {
            TaskAcceptance::None => false,
            TaskAcceptance::Single => {
                matches!(self.active_slot, TaskSlotState::Empty) && self.pending_count == 0
            }
            TaskAcceptance::Queue => self.pending_count < self.queue_depth,
        }
    }

    pub(crate) fn enqueue(&mut self, task_id: &str, context_id: &str) {
        self.pending_count += 1;
        self.record_state(task_id, TaskState::Submitted, context_id.to_string());
    }

    /// Take back a task [`Self::enqueue`] just enqueued, which the task loop never received:
    /// its queue slot, its state, its revision, its status time and its owner.
    pub(crate) fn withdraw(&mut self, task_id: &str) {
        self.pending_count = self.pending_count.saturating_sub(1);
        self.history.remove(task_id);
        self.revisions.remove(task_id);
        self.status_times.remove(task_id);
        self.owners.remove(task_id);
    }

    /// Record `state` for `task_id` in `context_id`, under the next revision. Every write to
    /// [`Self::history`] goes through here, so no state changes without moving the task to the
    /// front of `ListTasks`.
    pub(crate) fn record_state(&mut self, task_id: &str, state: TaskState, context_id: String) {
        self.history
            .insert(task_id.to_string(), (state, context_id));
        self.touch(task_id);
    }

    /// Stamp `task_id` with the next revision and the current instant, together, so its status
    /// timestamp moves exactly when its place in `ListTasks` does.
    fn touch(&mut self, task_id: &str) {
        self.revision += 1;
        self.revisions.insert(task_id.to_string(), self.revision);
        self.status_times.insert(task_id.to_string(), status_now());
    }

    /// The instant of `task_id`'s last change, or `None` for a task this registry does not hold.
    fn status_time(&self, task_id: &str) -> Option<DateTime<Utc>> {
        self.status_times.get(task_id).copied()
    }

    /// Record that the door credential `credential` started `task_id`. Called under the lock the
    /// task was enqueued under, so no `GetTask` can see the task without its owner.
    pub(crate) fn record_owner(&mut self, task_id: &str, credential: &str) {
        self.owners
            .insert(task_id.to_string(), credential.to_string());
    }

    /// The door credential that started `task_id`, or `None` for a task the operator, a caller of
    /// a public door or the runtime itself started.
    pub(crate) fn owner(&self, task_id: &str) -> Option<&str> {
        self.owners.get(task_id).map(String::as_str)
    }

    /// Record how `task_id` ended: its final status's `message`, and its `response` when it
    /// completed. A response on any other state is not kept. `no_answer` is whether it ended
    /// through `end-without-answer`, `no_answer_below` the `(member, status)` of each member
    /// further down that gave it no answer, and `running_commands` the background shell commands
    /// still running when it ended.
    pub(crate) fn record_ending(
        &mut self,
        task_id: &str,
        message: &str,
        response: Option<&str>,
        no_answer: bool,
        no_answer_below: &[(String, String)],
        running_commands: &[RunningCommand],
    ) {
        let completed = matches!(self.history.get(task_id), Some((TaskState::Completed, _)));
        self.endings.insert(
            task_id.to_string(),
            TaskEnding {
                message: message.to_string(),
                response: response.filter(|_| completed).map(str::to_string),
                no_answer,
                no_answer_below: no_answer_below.to_vec(),
                running_commands: running_commands.to_vec(),
            },
        );
        self.touch(task_id);
    }

    /// `lane` is the lane the queue selected this task out of, so what the registry reports is
    /// what the selection did.
    pub(crate) fn start_task(&mut self, task_id: String, context_id: String, lane: TaskLane) {
        debug_assert!(self.pending_count > 0);
        self.pending_count -= 1;
        self.record_state(&task_id, TaskState::Working, context_id.clone());
        self.active_slot = TaskSlotState::Running {
            task_id,
            context_id,
            lane,
        };
    }

    /// The lane of the task the capsule is running, or `None` when no task is running.
    ///
    /// `Some` is what stops the lane queue yielding anything, so a `Done` or `Empty` slot must
    /// answer `None`.
    pub(crate) fn active_lane(&self) -> Option<TaskLane> {
        match self.active_slot {
            TaskSlotState::Running { lane, .. } => Some(lane),
            TaskSlotState::Empty | TaskSlotState::Done => None,
        }
    }

    /// End the running task in `final_state`, and return the state actually recorded: an
    /// accepted cancel already in the history wins over whatever is passed. `None` when no task
    /// is running, which records nothing.
    pub(crate) fn finish_task(&mut self, final_state: TaskState) -> Option<TaskState> {
        let TaskSlotState::Running {
            ref task_id,
            ref context_id,
            ..
        } = self.active_slot
        else {
            return None;
        };
        let (tid, cid) = (task_id.clone(), context_id.clone());
        self.input_waiters.remove(&tid);
        self.input_timeouts.remove(&tid);
        self.cancels.remove(&tid);
        self.standing_answers.remove(&tid);
        self.stopped_at_deadline.remove(&tid);
        self.canceled_at_deadline.remove(&tid);
        // An accepted cancel is final. A turn a person stopped must never later read as one
        // that ran to completion, so the outcome the loop reports loses to the one already
        // recorded.
        let final_state = match self.history.get(&tid) {
            Some((TaskState::Canceled, _)) => TaskState::Canceled,
            _ => final_state,
        };
        self.record_state(&tid, final_state.clone(), cid);
        self.active_slot = TaskSlotState::Done;
        Some(final_state)
    }

    /// Record that `task_id`'s `request-input` wait passed `lifecycle.input_timeout_secs`
    /// without ending the task: the waiter is dropped, an `input-required` task goes back to
    /// `working`, and the timeout is marked for [`Self::take_input_timeout`]. The task's
    /// terminal state is the reopen loop's to record, once its `on-task-end` hooks have run.
    pub(crate) fn record_input_timeout(&mut self, task_id: &str) {
        self.input_waiters.remove(task_id);
        if let Some((TaskState::InputRequired, ctx)) = self.history.get(task_id).cloned() {
            self.record_state(task_id, TaskState::Working, ctx);
        }
        self.input_timeouts.insert(task_id.to_string());
    }

    /// Whether `task_id` has an unread input-wait timeout, clearing the mark. Read by the agent
    /// loop at each turn boundary, so the attempt that timed out ends and a reopened attempt of
    /// the same task does not end on the same mark.
    pub(crate) fn take_input_timeout(&mut self, task_id: &str) -> bool {
        self.input_timeouts.remove(task_id)
    }

    /// Transition the active task to InputRequired, storing the prompt and the
    /// oneshot sender that will deliver the external response.
    pub(crate) fn set_input_required(
        &mut self,
        task_id: &str,
        prompt: String,
        tx: oneshot::Sender<String>,
    ) -> Result<(), &'static str> {
        match &self.active_slot {
            TaskSlotState::Running {
                task_id: active_id, ..
            } if active_id == task_id => {}
            _ => return Err("task is not the active running task"),
        }
        let state = self.history.get(task_id).map(|(s, _)| s);
        if !matches!(state, Some(TaskState::Working)) {
            return Err("task is not in working state");
        }
        if let Some((_, ctx)) = self.history.get(task_id).cloned() {
            self.record_state(task_id, TaskState::InputRequired, ctx);
        }
        self.input_waiters.insert(task_id.to_string(), (prompt, tx));
        Ok(())
    }

    /// Deliver external input to an input-required task, transitioning it back to Working.
    pub(crate) fn deliver_input(
        &mut self,
        task_id: &str,
        text: String,
    ) -> Result<(), &'static str> {
        let state = self.history.get(task_id).map(|(s, _)| s);
        if !matches!(state, Some(TaskState::InputRequired)) {
            return Err("task is not in input-required state");
        }
        let Some((_, tx)) = self.input_waiters.remove(task_id) else {
            return Err("no input waiter found for task");
        };
        if let Some((_, ctx)) = self.history.get(task_id).cloned() {
            self.record_state(task_id, TaskState::Working, ctx);
        }
        // Sending may fail if the receiver was dropped (timeout path), which is fine.
        let _ = tx.send(text);
        Ok(())
    }

    /// The cancel signal for `task_id`, minting one if nobody has asked yet.
    ///
    /// Handed to whatever has to race against a cancel — the driver call, the input wait, the
    /// delegation wait — and to the door, which raises it. A signal for a task that never runs
    /// costs one entry and is dropped with the task's terminal state.
    pub(crate) fn cancel_watch(&mut self, task_id: &str) -> CancelSignal {
        self.cancels.entry(task_id.to_string()).or_default().clone()
    }

    /// Whether this task's recorded state is `Canceled`.
    ///
    /// Read at the task loop's activation step, which is what keeps a task cancelled while it was
    /// still `submitted` from ever starting.
    pub(crate) fn is_canceled(&self, task_id: &str) -> bool {
        matches!(self.history.get(task_id), Some((TaskState::Canceled, _)))
    }

    /// Stop one task: record `Canceled` and raise its signal.
    ///
    /// Called on the door's connection task and never waits for the agent loop to acknowledge —
    /// the state is written here, so a `GetTask` that lands next already reads `canceled`
    /// whatever the loop is in the middle of.
    ///
    /// A cancelled task that was still `submitted` gives its queue slot back immediately: it will
    /// be skipped when the loop reaches it, so holding capacity for it would refuse work the
    /// capsule can do.
    pub(crate) fn request_cancel(&mut self, task_id: &str) -> CancelOutcome {
        let Some((state, context_id)) = self.history.get(task_id).cloned() else {
            return CancelOutcome::Unknown;
        };
        if state.is_terminal() {
            return CancelOutcome::AlreadyTerminal;
        }
        if matches!(state, TaskState::Submitted) {
            self.pending_count = self.pending_count.saturating_sub(1);
        }
        self.record_state(task_id, TaskState::Canceled, context_id);
        self.cancel_watch(task_id).cancel();
        CancelOutcome::Accepted
    }

    /// Mark `task_id` as holding a standing answer: an earlier turn end's answer, which the task
    /// keeps while `lifecycle.keep_working` sends it on or `lifecycle.done_check` checks it.
    pub(crate) fn mark_standing_answer(&mut self, task_id: &str) {
        self.standing_answers.insert(task_id.to_string());
    }

    /// Drop `task_id`'s standing-answer mark, as a reopen does: the reopened attempt has no
    /// answer of its own yet.
    pub(crate) fn clear_standing_answer(&mut self, task_id: &str) {
        self.standing_answers.remove(task_id);
    }

    /// Whether the session deadline stopped `task_id` while it held a standing answer, leaving
    /// its state for the task loop to record.
    pub(crate) fn stopped_at_deadline(&self, task_id: &str) -> bool {
        self.stopped_at_deadline.contains(task_id)
    }

    /// Whether the session deadline cancelled `task_id`: its cancel was accepted by
    /// [`Self::cancel_every_live`] at the deadline, not by a person or another termination first.
    pub(crate) fn canceled_at_deadline(&self, task_id: &str) -> bool {
        self.canceled_at_deadline.contains(task_id)
    }

    /// Cancel every task that is not yet terminal, and return the ids that were cancelled, sorted.
    ///
    /// Each goes through [`Self::request_cancel`], so a running task's signal is raised and a
    /// `submitted` task's queue slot is given back. A second call returns nothing: every task the
    /// first one touched is `Canceled`, which is terminal.
    ///
    /// With `at_deadline`, which the session deadline passes, a task holding a standing answer
    /// ([`Self::mark_standing_answer`]) only has its signal raised and is marked
    /// [`Self::stopped_at_deadline`]: its history is left as it is, it is not in the returned ids,
    /// and the task loop ends it with that answer. Every task it does cancel is marked
    /// [`Self::canceled_at_deadline`].
    pub(crate) fn cancel_every_live(&mut self, at_deadline: bool) -> Vec<String> {
        let live: Vec<String> = self
            .history
            .iter()
            .filter(|(_, (state, _))| !state.is_terminal())
            .map(|(task_id, _)| task_id.clone())
            .collect();
        let (spared, live): (Vec<String>, Vec<String>) = live
            .into_iter()
            .partition(|task_id| at_deadline && self.standing_answers.contains(task_id));
        for task_id in spared {
            self.cancel_watch(&task_id).cancel();
            self.stopped_at_deadline.insert(task_id);
        }
        let mut canceled: Vec<String> = live
            .into_iter()
            .filter(|task_id| matches!(self.request_cancel(task_id), CancelOutcome::Accepted))
            .collect();
        if at_deadline {
            self.canceled_at_deadline.extend(canceled.iter().cloned());
        }
        // `history` is a `HashMap`, so without this the same two tasks come back in a different
        // order on every call and nothing can diff two stops.
        canceled.sort();
        canceled
    }

    /// Stop taking work, and end every task still `submitted` in `Rejected`. Returns the refused
    /// tasks' `(task_id, context_id)`, sorted by task id, which for UUIDv7 ids is acceptance order.
    ///
    /// Taken under the same lock the door enqueues under, so no task can be enqueued after the
    /// call returns: [`Self::can_accept`] answers `false` from here on. Only `Submitted` tasks are
    /// touched. A `Canceled` task keeps its state, and a task that ran keeps the state it ended in.
    /// No turn ran, so the resource generation does not advance. A second call returns nothing.
    pub(crate) fn close_to_new_work(&mut self) -> Vec<(String, String)> {
        self.closed = true;
        let mut refused: Vec<(String, String)> = self
            .history
            .iter()
            .filter(|(_, (state, _))| matches!(state, TaskState::Submitted))
            .map(|(task_id, (_, context_id))| (task_id.clone(), context_id.clone()))
            .collect();
        refused.sort();
        for (task_id, context_id) in &refused {
            self.record_state(task_id, TaskState::Rejected, context_id.clone());
            self.cancels.remove(task_id);
        }
        self.pending_count = self.pending_count.saturating_sub(refused.len());
        refused
    }

    /// Whether [`Self::close_to_new_work`] has run. The door reads it to tell a closing session
    /// from a busy one when it refuses a task.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether `task_id`'s ending is recorded, which happens under the lock its terminal state is
    /// written under on every path but a cancel, whose ending follows once the task stops.
    pub(crate) fn has_ending(&self, task_id: &str) -> bool {
        self.endings.contains_key(task_id)
    }

    /// Return the prompt stored for an input-required task.
    #[allow(dead_code)] // used in unit tests
    pub(crate) fn get_input_prompt(&self, task_id: &str) -> Option<&str> {
        self.input_waiters
            .get(task_id)
            .map(|(prompt, _)| prompt.as_str())
    }

    /// `task_id` as `GetTask` reports it.
    ///
    /// Its `status.timestamp` is the instant of the task's last change. An `input-required` task
    /// carries its prompt as a `prompt` artifact. A terminal task carries its final status's
    /// message as `status.message`, and a `completed` one its response as a `response` artifact,
    /// each when it has one.
    pub(crate) fn get_task(&self, task_id: &str) -> Option<A2aTask> {
        self.history.get(task_id).map(|(state, context_id)| {
            let artifact =
                |name: &str, text: &str| A2aArtifact::named(name, vec![Part::text(text)]);
            let ending = self.endings.get(task_id).filter(|_| state.is_terminal());
            let artifacts = match state {
                TaskState::InputRequired => self
                    .input_waiters
                    .get(task_id)
                    .map(|(prompt, _)| vec![artifact(PROMPT_ARTIFACT, prompt)]),
                TaskState::Completed => ending
                    .and_then(|ending| ending.response.as_deref())
                    .filter(|response| !response.is_empty())
                    .map(|response| vec![artifact(RESPONSE_ARTIFACT, response)]),
                _ => None,
            };
            let message = ending
                .filter(|ending| !ending.message.is_empty())
                .map(|ending| StatusMessage::agent(task_id, context_id, &ending.message));
            A2aTask {
                id: task_id.to_string(),
                context_id: context_id.clone(),
                status: TaskStatus::at(
                    state.clone(),
                    message,
                    self.status_time(task_id).unwrap_or_else(status_now),
                ),
                artifacts,
                metadata: ending.and_then(TaskEnding::metadata),
            }
        })
    }

    /// The block naming each task in `reference_task_ids` as it stands now, appended to a message
    /// that references them, or `None` when it references none. `visible` says whether the sender
    /// may see a task: one it may not reads exactly as one this registry never held.
    ///
    /// Each task is one line, `- <id>: <state>` in murmur's word. An ended task other than a
    /// `completed` one appends what its final status said; a `completed` one with a response is
    /// followed by the response in a code fence labelled `response`.
    pub(crate) fn reference_block(
        &self,
        reference_task_ids: &[String],
        visible: impl Fn(&str) -> bool,
    ) -> Option<String> {
        if reference_task_ids.is_empty() {
            return None;
        }
        let lines: Vec<String> = reference_task_ids
            .iter()
            .map(|task_id| {
                let Some((state, _)) = self.history.get(task_id).filter(|_| visible(task_id))
                else {
                    return format!("- {task_id}: not a task this capsule holds");
                };
                let mut line = format!("- {task_id}: {}", state.as_str());
                let ending = self.endings.get(task_id).filter(|_| state.is_terminal());
                match (state, ending) {
                    (TaskState::Completed, Some(ending)) => {
                        if let Some(response) =
                            ending.response.as_deref().filter(|text| !text.is_empty())
                        {
                            line.push('\n');
                            line.push_str(&code_fence(RESPONSE_ARTIFACT, response));
                        }
                    }
                    (_, Some(ending)) if !ending.message.is_empty() => {
                        line.push_str(": ");
                        line.push_str(&ending.message.replace(['\r', '\n'], " "));
                    }
                    _ => {}
                }
                line
            })
            .collect();
        Some(format!("Referenced tasks:\n{}", lines.join("\n")))
    }

    /// One `ListTasks` page: the tasks `visible` lets the caller see that match `filter`, newest
    /// revision first, the first `page_size` of those strictly after `after`.
    ///
    /// Each task is exactly [`Self::get_task`]'s. `next` is the position of the page's last task
    /// when a matching task lies past it, and `total` counts every visible match, before paging.
    /// A task whose state changes during a walk takes a newer revision and moves ahead of the
    /// cursor, so a walk returns every match exactly once only while no match changes.
    pub(crate) fn list_tasks(
        &self,
        filter: &TaskFilter,
        after: Option<&ListCursor>,
        page_size: usize,
        visible: impl Fn(&str) -> bool,
    ) -> TaskPage {
        let mut matching: Vec<ListCursor> = self
            .history
            .iter()
            .filter(|(task_id, (state, context_id))| {
                filter.matches(state, context_id, self.status_time(task_id)) && visible(task_id)
            })
            .map(|(task_id, _)| ListCursor {
                revision: self.revisions.get(task_id).copied().unwrap_or_default(),
                task_id: task_id.clone(),
            })
            .collect();
        matching.sort_unstable_by(|a, b| b.cmp(a));
        let total = matching.len();
        let rest: Vec<ListCursor> = matching
            .into_iter()
            .filter(|position| after.is_none_or(|after| position < after))
            .collect();
        let page: Vec<&ListCursor> = rest.iter().take(page_size).collect();
        let next = (rest.len() > page.len())
            .then(|| page.last().map(|last| (*last).clone()))
            .flatten();
        TaskPage {
            tasks: page
                .iter()
                .filter_map(|position| self.get_task(&position.task_id))
                .collect(),
            next,
            total,
        }
    }
}

/// Which tasks a `ListTasks` call asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskFilter {
    /// Only the tasks in this context, or the tasks of every context when `None`.
    pub context_id: Option<String>,
    pub state: StateFilter,
    /// Only the tasks whose status time is at or after this instant (`statusTimestampAfter`), or
    /// every task when `None`.
    pub status_after: Option<DateTime<Utc>>,
}

impl TaskFilter {
    fn matches(&self, state: &TaskState, context_id: &str, at: Option<DateTime<Utc>>) -> bool {
        let in_context = self.context_id.as_deref().is_none_or(|c| c == context_id);
        let in_state = match &self.state {
            StateFilter::Any => true,
            StateFilter::Only(wanted) => wanted == state,
            StateFilter::Nothing => false,
        };
        let in_time = self
            .status_after
            .is_none_or(|after| at.is_some_and(|at| at >= after));
        in_context && in_state && in_time
    }
}

/// The `status` a `ListTasks` call filters by.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StateFilter {
    /// No `status`, or `TASK_STATE_UNSPECIFIED`.
    Any,
    /// Only tasks in this state.
    Only(TaskState),
    /// A state no task here is ever in: `TASK_STATE_AUTH_REQUIRED`.
    Nothing,
}

impl StateFilter {
    /// The filter a `status` naming the v1.0 `TaskState` `wire` asks for, or `None` when `wire`
    /// is not a `TaskState` name.
    pub(crate) fn from_wire_name(wire: &str) -> Option<StateFilter> {
        match wire {
            "TASK_STATE_UNSPECIFIED" => Some(StateFilter::Any),
            "TASK_STATE_AUTH_REQUIRED" => Some(StateFilter::Nothing),
            wire => TaskState::from_wire_name(wire).map(StateFilter::Only),
        }
    }
}

/// A place in the `ListTasks` order: a task's revision and its id. Revisions are unique, so the
/// order is total; the id only makes the position self-describing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ListCursor {
    pub revision: u64,
    pub task_id: String,
}

impl ListCursor {
    /// The `nextPageToken` naming this position: `<revision>:<task_id>` in unpadded base64url.
    /// Clients treat it as opaque.
    pub(crate) fn encode(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{}:{}", self.revision, self.task_id))
    }

    /// The position a `pageToken` names, or `None` for a string [`Self::encode`] did not produce.
    pub(crate) fn decode(token: &str) -> Option<ListCursor> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let (revision, task_id) = text.split_once(':')?;
        let revision = revision
            .parse()
            .ok()
            .filter(|_| !revision.starts_with('+'))?;
        (!task_id.is_empty()).then(|| ListCursor {
            revision,
            task_id: task_id.to_string(),
        })
    }
}

/// One `ListTasks` page, as [`TaskRegistry::list_tasks`] returns it.
#[derive(Debug, Clone)]
pub(crate) struct TaskPage {
    pub tasks: Vec<A2aTask>,
    /// Where the next page starts, or `None` on the last page.
    pub next: Option<ListCursor>,
    /// How many tasks the caller sees that match the filter.
    pub total: usize,
}

// ── Incoming task (HTTP server → main thread) ─────────────────────────────────

#[derive(Debug)]
pub(crate) struct IncomingTask {
    pub task_id: String,
    pub context_id: String,
    pub message_id: String,
    pub message_text: String,
    pub traceparent: Option<String>,
    /// Why this task woke the capsule, classified at the door from the request headers. Every
    /// inbound task has one: a caller that claims nothing is an untrusted `event`.
    pub provenance: TaskProvenance,
    /// Where the task came from, as it appears on its `task_start` record: [`SOURCE_A2A`] for
    /// anything that arrived over the peer door, [`SOURCE_TASK_MD`] for the launch's own task,
    /// and [`SOURCE_DETACHED_SHELL`] or [`SOURCE_DETACHED_LOST`] for a report the runtime produced
    /// locally. The `a2a_task_received` record is written only for the first.
    pub source: &'static str,
    /// Whether the message that started this task asked the capsule to drop the harness session
    /// its context names before the turn: [`IncomingMessage::forget_session`], or for the launch's
    /// own task, `mur run --forget-session`. `false` for every task whose message did not ask, and
    /// for every report the runtime enqueued for itself.
    pub forget_session: bool,
    /// The formation member that called, when the door let this task in on a formation token.
    /// Recorded as `a2a_task_received.caller_member`; `None` for every other task.
    pub caller_member: Option<String>,
    /// Who the door let this task in as, recorded as `a2a_task_received.caller`. `Some` exactly
    /// for a [`SOURCE_A2A`] task; `None` for the launch's own task and every report the runtime
    /// enqueued for itself, none of which crossed the door.
    pub caller: Option<crate::identity::TaskCaller>,
    /// The message's `referenceTaskIds`, each once, recorded as `task_start.reference_task_ids`.
    /// Empty for every task that did not arrive over the door.
    pub reference_task_ids: Vec<String>,
    /// The kind of each part of the message, recorded as `task_start.part_kinds`. Empty for every
    /// task that did not arrive over the door.
    pub part_kinds: Vec<&'static str>,
}

/// The `source` of a task that arrived over the A2A door.
pub(crate) const SOURCE_A2A: &str = "a2a";

/// The `source` of a launch's own task: `--task`, or a workdir `task.md`/`input.txt` imported at
/// session start. It is a task on the door from the moment the door is announced, but it never
/// crossed the door, so it gets no `a2a_task_received` record.
pub(crate) const SOURCE_TASK_MD: &str = "task_md";

/// The `source` of a task the runtime enqueued for itself when a demoted shell command finished.
pub(crate) const SOURCE_DETACHED_SHELL: &str = "detached_shell";

/// The `source` of a task a resumed launch enqueued for itself to report demoted commands the
/// session it resumes never accounted for. Distinct from [`SOURCE_DETACHED_SHELL`] because the
/// two say opposite things: one carries a result, the other says no result exists.
pub(crate) const SOURCE_DETACHED_LOST: &str = "detached_lost";

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> TaskRegistry {
        TaskRegistry::new(1, TaskAcceptance::Single)
    }

    fn running_registry(task_id: &str) -> TaskRegistry {
        let mut r = make_registry();
        r.enqueue(task_id, "ctx_001");
        r.start_task(task_id.to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r
    }

    #[test]
    fn set_input_required_transitions_state() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        assert!(r
            .set_input_required("tsk_001", "which branch?".into(), tx)
            .is_ok());
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::InputRequired);
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts[0].artifact_id, "prompt");
        assert_eq!(artifacts[0].first_text(), Some("which branch?"));
    }

    #[test]
    fn set_input_required_fails_for_wrong_task() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        assert!(r
            .set_input_required("tsk_other", "prompt".into(), tx)
            .is_err());
    }

    #[test]
    fn deliver_input_transitions_back_to_working() {
        let mut r = running_registry("tsk_001");
        let (tx, mut rx) = oneshot::channel();
        r.set_input_required("tsk_001", "which branch?".into(), tx)
            .unwrap();
        r.deliver_input("tsk_001", "main".to_string()).unwrap();
        // Oneshot should have received the value
        assert_eq!(rx.try_recv().unwrap(), "main");
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::Working);
        assert!(task.artifacts.is_none());
    }

    #[test]
    fn deliver_input_fails_when_not_input_required() {
        let mut r = running_registry("tsk_001");
        assert!(r.deliver_input("tsk_001", "text".into()).is_err());
    }

    #[test]
    fn an_input_required_task_carries_its_prompt_and_nothing_else() {
        let mut r = running_registry("tsk_1");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_1", "Which file?".to_string(), tx)
            .unwrap();
        let task = r.get_task("tsk_1").unwrap();
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(
            serde_json::to_value(&artifacts[0]).unwrap(),
            serde_json::json!({"artifactId": "prompt", "name": "prompt",
                "parts": [{"text": "Which file?", "mediaType": "text/plain"}]})
        );
        assert!(task.status.message.is_none());
    }

    /// A live task carries nothing beyond its state: no artifact and no status message.
    #[test]
    fn a_working_task_carries_no_artifact_and_no_message() {
        let r = running_registry("tsk_1");
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_bare_status(&task["status"], "TASK_STATE_WORKING");
        assert_eq!(
            task,
            serde_json::json!({"id": "tsk_1", "contextId": "ctx_001", "status": {
                "state": "TASK_STATE_WORKING",
                "timestamp": timestamp_of(&r, "tsk_1"),
            }})
        );
    }

    /// A completed task answers with its response as the `response` artifact and its final
    /// message as `status.message`, an A2A message from the agent.
    #[test]
    fn a_completed_task_carries_its_response_artifact_and_status_message() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_1", "done", Some("the answer is 4"), false, &[], &[]);
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(
            task,
            serde_json::json!({
                "id": "tsk_1",
                "contextId": "ctx_001",
                "status": {
                    "state": "TASK_STATE_COMPLETED",
                    "message": {
                        "messageId": "msg_tsk_1_status",
                        "contextId": "ctx_001",
                        "taskId": "tsk_1",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "done", "mediaType": "text/plain"}],
                    },
                    "timestamp": status_timestamp(r.status_times["tsk_1"]),
                },
                "artifacts": [{
                    "artifactId": "response",
                    "name": "response",
                    "parts": [{"text": "the answer is 4", "mediaType": "text/plain"}],
                }],
            })
        );
    }

    /// A task that ended with background commands running names each in
    /// `metadata.murmur.runningCommands`, its text cut to 200 characters; a task with none
    /// carries no such key.
    #[test]
    fn a_task_ending_with_commands_running_names_them_in_its_metadata() {
        let long = "x".repeat(250);
        let running = [
            RunningCommand {
                work_id: "wrk_0a".to_string(),
                binary: "/usr/bin/bash".to_string(),
                command: "sleep 300".to_string(),
                running_ms: 12_000,
            },
            RunningCommand {
                work_id: "wrk_0b".to_string(),
                binary: "/usr/bin/bash".to_string(),
                command: long.clone(),
                running_ms: 3_000,
            },
        ];
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_1", "done", Some("4"), false, &[], &running);
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(
            task["metadata"],
            serde_json::json!({"murmur": {"runningCommands": [
                {"workId": "wrk_0a", "binary": "/usr/bin/bash", "command": "sleep 300",
                 "runningMs": 12_000},
                {"workId": "wrk_0b", "binary": "/usr/bin/bash",
                 "command": format!("{}…", &long[..200]), "runningMs": 3_000},
            ]}})
        );

        let mut r = running_registry("tsk_2");
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_2", "done", Some("4"), false, &[], &[]);
        let task = serde_json::to_value(r.get_task("tsk_2").unwrap()).unwrap();
        assert!(task.get("metadata").is_none(), "{task}");
    }

    /// A task ended through `end-without-answer` carries `metadata.murmur.noAnswer` and the
    /// members further down that gave it none; a completed task with members missing below it
    /// carries its response and `noAnswerBelow` but no `noAnswer`; any other ending carries no
    /// `metadata` at all.
    #[test]
    fn a_no_answer_ending_carries_its_metadata_and_no_other_ending_does() {
        let below = [("q".to_string(), "timed_out".to_string())];
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Failed);
        r.record_ending("tsk_1", "q gave no answer", None, true, &below, &[]);
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        assert_eq!(
            task["status"]["message"]["parts"][0]["text"],
            "q gave no answer"
        );
        assert_eq!(
            task["metadata"],
            serde_json::json!({"murmur": {
                "noAnswer": true,
                "noAnswerBelow": [{"member": "q", "status": "timed_out"}],
            }})
        );

        let mut r = running_registry("tsk_2");
        r.finish_task(TaskState::Completed);
        r.record_ending(
            "tsk_2",
            "task completed",
            Some("none came"),
            false,
            &below,
            &[],
        );
        let task = serde_json::to_value(r.get_task("tsk_2").unwrap()).unwrap();
        assert_eq!(
            task["artifacts"],
            serde_json::json!([{"artifactId": "response", "name": "response",
                "parts": [{"text": "none came", "mediaType": "text/plain"}]}])
        );
        assert_eq!(
            task["metadata"],
            serde_json::json!({"murmur": {
                "noAnswerBelow": [{"member": "q", "status": "timed_out"}],
            }})
        );

        for state in [TaskState::Completed, TaskState::Failed] {
            let mut r = running_registry("tsk_3");
            r.finish_task(state);
            r.record_ending("tsk_3", "ended", Some("4"), false, &[], &[]);
            let task = serde_json::to_value(r.get_task("tsk_3").unwrap()).unwrap();
            assert!(task.get("metadata").is_none(), "{task}");
        }
        let r = running_registry("tsk_4");
        let task = serde_json::to_value(r.get_task("tsk_4").unwrap()).unwrap();
        assert!(task.get("metadata").is_none(), "{task}");
    }

    /// A task that did not complete carries its message and no response, whatever was passed.
    #[test]
    fn a_failed_task_carries_its_status_message_and_no_response() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Failed);
        r.record_ending(
            "tsk_1",
            "the driver failed",
            Some("partial"),
            false,
            &[],
            &[],
        );
        let task = r.get_task("tsk_1").unwrap();
        assert!(task.artifacts.is_none());
        let message = task.status.message.unwrap();
        assert_eq!(message.role, Role::Agent);
        assert_eq!(message.text(), Some("the driver failed"));
    }

    /// A completed task with no response text, or no recorded ending, carries no artifact.
    #[test]
    fn a_completed_task_without_a_response_carries_no_artifact() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Completed);
        assert!(r.get_task("tsk_1").unwrap().artifacts.is_none());
        r.record_ending("tsk_1", "ok", Some(""), false, &[], &[]);
        let task = r.get_task("tsk_1").unwrap();
        assert!(task.artifacts.is_none());
        assert_eq!(task.status.message.unwrap().text(), Some("ok"));
    }

    #[test]
    fn an_owner_is_recorded_per_task() {
        let mut r = make_registry();
        r.enqueue("tsk_a", "ctx_001");
        r.record_owner("tsk_a", "member:lead");
        r.enqueue("tsk_b", "ctx_001");
        assert_eq!(r.owner("tsk_a"), Some("member:lead"));
        assert_eq!(r.owner("tsk_b"), None);
        r.withdraw("tsk_a");
        assert_eq!(r.owner("tsk_a"), None);
        assert!(r.get_task("tsk_a").is_none());
        assert_eq!(r.pending_count, 1);
    }

    /// The ids of a page, in order.
    fn listed(page: &TaskPage) -> Vec<&str> {
        page.tasks.iter().map(|task| task.id.as_str()).collect()
    }

    fn every_task() -> TaskFilter {
        TaskFilter {
            context_id: None,
            state: StateFilter::Any,
            status_after: None,
        }
    }

    /// Every change to a task's state or ending moves it to the front of the listing.
    #[test]
    fn list_tasks_orders_by_the_last_change_newest_first() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        for id in ["tsk_a", "tsk_b", "tsk_c"] {
            r.enqueue(id, "ctx_001");
        }
        let all = |r: &TaskRegistry| -> Vec<String> {
            listed(&r.list_tasks(&every_task(), None, 10, |_| true))
                .into_iter()
                .map(str::to_string)
                .collect()
        };
        assert_eq!(all(&r), ["tsk_c", "tsk_b", "tsk_a"]);
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::User);
        assert_eq!(all(&r), ["tsk_a", "tsk_c", "tsk_b"]);
        r.request_cancel("tsk_b");
        assert_eq!(all(&r), ["tsk_b", "tsk_a", "tsk_c"]);
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_a", "which?".to_string(), tx)
            .unwrap();
        assert_eq!(all(&r), ["tsk_a", "tsk_b", "tsk_c"]);
        r.deliver_input("tsk_a", "left".to_string()).unwrap();
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_c", "", None, false, &[], &[]);
        assert_eq!(all(&r), ["tsk_c", "tsk_a", "tsk_b"]);
        r.record_ending("tsk_a", "done", Some("four"), false, &[], &[]);
        assert_eq!(all(&r), ["tsk_a", "tsk_c", "tsk_b"]);
        r.close_to_new_work();
        assert_eq!(all(&r), ["tsk_c", "tsk_a", "tsk_b"], "tsk_c was rejected");

        let page = r.list_tasks(&every_task(), None, 10, |_| true);
        for task in &page.tasks {
            assert_eq!(
                serde_json::to_value(task).unwrap(),
                serde_json::to_value(r.get_task(&task.id).unwrap()).unwrap()
            );
        }
    }

    /// Pages follow one another through the cursor; a task that changes during the walk moves
    /// ahead of the cursor and is not returned a second time.
    #[test]
    fn list_tasks_walks_pages_and_returns_a_task_changed_mid_walk_once() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        for id in ["tsk_1", "tsk_2", "tsk_3", "tsk_4", "tsk_5"] {
            r.enqueue(id, "ctx_001");
        }
        let first = r.list_tasks(&every_task(), None, 2, |_| true);
        assert_eq!(listed(&first), ["tsk_5", "tsk_4"]);
        assert_eq!(first.total, 5);
        let cursor = first.next.clone().expect("more tasks lie past the page");
        assert_eq!(ListCursor::decode(&cursor.encode()), Some(cursor.clone()));

        r.request_cancel("tsk_4");
        r.request_cancel("tsk_2");
        let second = r.list_tasks(&every_task(), Some(&cursor), 2, |_| true);
        assert_eq!(listed(&second), ["tsk_3", "tsk_1"], "tsk_2 moved ahead");
        assert_eq!(second.next, None);
        assert_eq!(second.total, 5);
        let fresh = r.list_tasks(&every_task(), None, 2, |_| true);
        assert_eq!(listed(&fresh), ["tsk_2", "tsk_4"]);

        let exact = r.list_tasks(&every_task(), None, 5, |_| true);
        assert_eq!(exact.tasks.len(), 5);
        assert_eq!(
            exact.next, None,
            "no task lies past a page that ends the list"
        );
    }

    /// The instant a status timestamp names, parsed back.
    fn instant(timestamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn timestamp_of(r: &TaskRegistry, task_id: &str) -> String {
        r.get_task(task_id).unwrap().status.timestamp.unwrap()
    }

    /// Long enough that two stamps taken either side of it differ at millisecond precision.
    fn a_tick() {
        std::thread::sleep(std::time::Duration::from_millis(3));
    }

    /// The status timestamp and the revision are stamped together: each change moves both, and
    /// nothing else moves either.
    #[test]
    fn a_status_timestamp_moves_with_the_revision() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        let submitted = timestamp_of(&r, "tsk_a");
        let submitted_revision = r.revisions["tsk_a"];
        assert_eq!(r.status_times["tsk_a"], instant(&submitted));

        a_tick();
        r.record_owner("tsk_a", "alice");
        assert_eq!(
            timestamp_of(&r, "tsk_a"),
            submitted,
            "an owner is no change"
        );
        assert_eq!(r.revisions["tsk_a"], submitted_revision);

        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::User);
        let working = timestamp_of(&r, "tsk_a");
        assert!(r.revisions["tsk_a"] > submitted_revision);
        assert!(
            instant(&working) > instant(&submitted),
            "{submitted} → {working}"
        );

        a_tick();
        r.finish_task(TaskState::Completed);
        let completed_revision = r.revisions["tsk_a"];
        let completed = timestamp_of(&r, "tsk_a");
        assert!(instant(&completed) > instant(&working));

        a_tick();
        r.record_ending("tsk_a", "done", Some("four"), false, &[], &[]);
        assert!(r.revisions["tsk_a"] > completed_revision);
        assert!(instant(&timestamp_of(&r, "tsk_a")) > instant(&completed));
    }

    /// GetTask's string is the stored instant exactly, so a cutoff copied from it includes the
    /// task: the comparison is "at or after".
    #[test]
    fn a_status_timestamp_round_trips_and_a_cutoff_equal_to_it_includes_the_task() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        let timestamp = timestamp_of(&r, "tsk_a");
        assert!(timestamp.ends_with('Z'), "{timestamp}");
        assert_eq!(
            timestamp.len(),
            "2026-10-13T09:00:00.123Z".len(),
            "{timestamp}"
        );
        assert_eq!(instant(&timestamp), r.status_times["tsk_a"]);
        let at = TaskFilter {
            status_after: Some(instant(&timestamp)),
            ..every_task()
        };
        assert_eq!(listed(&r.list_tasks(&at, None, 10, |_| true)), ["tsk_a"]);
        let after = TaskFilter {
            status_after: Some(instant(&timestamp) + chrono::Duration::milliseconds(1)),
            ..every_task()
        };
        assert!(r.list_tasks(&after, None, 10, |_| true).tasks.is_empty());
    }

    #[test]
    fn withdraw_drops_the_status_time() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        r.withdraw("tsk_a");
        assert!(!r.status_times.contains_key("tsk_a"));
        assert!(r.get_task("tsk_a").is_none());
    }

    /// `status_after` filters before paging, so `total` counts only the tasks it keeps and the
    /// cursor walks only those; it combines with the other filters.
    #[test]
    fn list_tasks_filters_by_status_time_before_paging() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        for id in ["tsk_1", "tsk_2", "tsk_3", "tsk_4"] {
            r.enqueue(id, "ctx_001");
        }
        r.request_cancel("tsk_3");
        let base = instant("2026-10-13T09:00:00Z");
        for (offset, id) in ["tsk_1", "tsk_2", "tsk_3", "tsk_4"].iter().enumerate() {
            r.status_times.insert(
                id.to_string(),
                base + chrono::Duration::seconds(offset as i64),
            );
        }
        let since = |seconds: i64| TaskFilter {
            status_after: Some(base + chrono::Duration::seconds(seconds)),
            ..every_task()
        };
        let first = r.list_tasks(&since(1), None, 2, |_| true);
        assert_eq!(listed(&first), ["tsk_3", "tsk_4"]);
        assert_eq!(first.total, 3);
        let second = r.list_tasks(&since(1), first.next.as_ref(), 2, |_| true);
        assert_eq!(listed(&second), ["tsk_2"]);
        assert_eq!((second.total, second.next), (3, None));

        let canceled_since = TaskFilter {
            state: StateFilter::Only(TaskState::Canceled),
            ..since(1)
        };
        let page = r.list_tasks(&canceled_since, None, 10, |_| true);
        assert_eq!((listed(&page), page.total), (vec!["tsk_3"], 1));
        assert_eq!(r.list_tasks(&since(4), None, 10, |_| true).total, 0);
        assert_eq!(r.list_tasks(&every_task(), None, 10, |_| true).total, 4);
    }

    /// The filters and the visibility predicate decide the page and the total alike.
    #[test]
    fn list_tasks_filters_by_context_state_and_visibility() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_one");
        r.record_owner("tsk_a", "alice");
        r.enqueue("tsk_b", "ctx_two");
        r.record_owner("tsk_b", "alice");
        r.enqueue("tsk_c", "ctx_one");
        r.record_owner("tsk_c", "bob");
        r.request_cancel("tsk_c");
        let alice = |task_id: &str| r.owner(task_id) == Some("alice");
        let page = r.list_tasks(&every_task(), None, 10, alice);
        assert_eq!((listed(&page), page.total), (vec!["tsk_b", "tsk_a"], 2));
        let in_one = TaskFilter {
            context_id: Some("ctx_one".to_string()),
            state: StateFilter::Any,
            status_after: None,
        };
        let page = r.list_tasks(&in_one, None, 10, |_| true);
        assert_eq!((listed(&page), page.total), (vec!["tsk_c", "tsk_a"], 2));
        let canceled = TaskFilter {
            context_id: None,
            state: StateFilter::Only(TaskState::Canceled),
            status_after: None,
        };
        let page = r.list_tasks(&canceled, None, 10, |_| true);
        assert_eq!(listed(&page), ["tsk_c"]);
        let page = r.list_tasks(&canceled, None, 10, alice);
        assert_eq!((page.tasks.len(), page.total, page.next), (0, 0, None));
        let nothing = TaskFilter {
            context_id: None,
            state: StateFilter::Nothing,
            status_after: None,
        };
        assert_eq!(r.list_tasks(&nothing, None, 10, |_| true).total, 0);
    }

    #[test]
    fn list_cursor_reads_only_what_it_wrote() {
        let cursor = ListCursor {
            revision: 42,
            task_id: "tsk_01a1".to_string(),
        };
        let token = cursor.encode();
        assert!(!token.contains('='), "{token}");
        assert_eq!(ListCursor::decode(&token), Some(cursor));
        use base64::Engine as _;
        let encode = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        for garbage in [
            "not-a-token".to_string(),
            "".to_string(),
            "!!".to_string(),
            encode("42"),
            encode("42:"),
            encode("x:tsk_1"),
            encode("-1:tsk_1"),
            encode("+1:tsk_1"),
        ] {
            assert_eq!(ListCursor::decode(&garbage), None, "{garbage}");
        }
    }

    #[test]
    fn list_state_filter_reads_every_v1_state_name() {
        for state in TaskState::ALL {
            assert_eq!(
                TaskState::from_wire_name(state.wire_name()),
                Some(state.clone())
            );
            assert_eq!(
                StateFilter::from_wire_name(state.wire_name()),
                Some(StateFilter::Only(state))
            );
        }
        assert_eq!(
            StateFilter::from_wire_name("TASK_STATE_UNSPECIFIED"),
            Some(StateFilter::Any)
        );
        assert_eq!(
            StateFilter::from_wire_name("TASK_STATE_AUTH_REQUIRED"),
            Some(StateFilter::Nothing)
        );
        for other in ["done", "completed", "task_state_completed", ""] {
            assert_eq!(StateFilter::from_wire_name(other), None, "{other}");
            assert_eq!(TaskState::from_wire_name(other), None, "{other}");
        }
    }

    #[test]
    fn request_cancel_on_a_running_task_records_canceled() {
        let mut r = running_registry("tsk_001");
        let signal = r.cancel_watch("tsk_001");
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::Accepted);
        assert!(signal.is_canceled());
        assert!(r.is_canceled("tsk_001"));
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
    }

    #[test]
    fn request_cancel_on_a_completed_task_leaves_it_alone() {
        let mut r = running_registry("tsk_001");
        r.finish_task(TaskState::Completed);
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::AlreadyTerminal);
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Completed
        );
        // And a second cancel of an already-cancelled task says the same thing.
        let mut r = running_registry("tsk_002");
        assert_eq!(r.request_cancel("tsk_002"), CancelOutcome::Accepted);
        assert_eq!(r.request_cancel("tsk_002"), CancelOutcome::AlreadyTerminal);
    }

    #[test]
    fn cancel_every_live_cancels_live_tasks_in_order_and_leaves_terminal_ones() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_c", "ctx_001");
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_done", "ctx_001");
        r.start_task("tsk_done".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r.finish_task(TaskState::Completed);
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        let running = r.cancel_watch("tsk_a");

        assert_eq!(r.cancel_every_live(false), vec!["tsk_a", "tsk_c"]);
        assert!(running.is_canceled());
        assert!(r.is_canceled("tsk_c"));
        assert_eq!(
            r.get_task("tsk_done").unwrap().status.state,
            TaskState::Completed
        );
        assert!(r.cancel_every_live(false).is_empty());
    }

    /// The deadline spares a running task holding a standing answer: its signal is raised, its
    /// history stays non-terminal, and the state the task loop then reports is the one recorded.
    #[test]
    fn cancel_every_live_at_the_deadline_spares_a_task_holding_a_standing_answer() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_b", "ctx_001");
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        let running = r.cancel_watch("tsk_a");
        r.mark_standing_answer("tsk_a");

        assert_eq!(r.cancel_every_live(true), vec!["tsk_b"]);
        assert!(running.is_canceled());
        assert!(!r.is_canceled("tsk_a"));
        assert!(!r.get_task("tsk_a").unwrap().status.state.is_terminal());
        assert!(r.stopped_at_deadline("tsk_a"));
        assert!(
            r.is_canceled("tsk_b"),
            "an unmarked task is cancelled at once"
        );

        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Completed)
        );
        assert_eq!(
            r.get_task("tsk_a").unwrap().status.state,
            TaskState::Completed
        );
        assert!(
            !r.stopped_at_deadline("tsk_a"),
            "finish_task clears the mark"
        );
    }

    /// The deadline marks every task it cancels, and only those: a task a person cancelled first
    /// keeps its own cancel, and no other termination marks anything.
    #[test]
    fn cancel_every_live_at_the_deadline_marks_the_tasks_it_cancels() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_b", "ctx_001");
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        assert_eq!(r.request_cancel("tsk_b"), CancelOutcome::Accepted);

        assert_eq!(r.cancel_every_live(true), vec!["tsk_a"]);
        assert!(r.canceled_at_deadline("tsk_a"));
        assert!(
            !r.canceled_at_deadline("tsk_b"),
            "a person's cancel came first"
        );
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Canceled)
        );
        assert!(
            !r.canceled_at_deadline("tsk_a"),
            "finish_task clears the mark"
        );

        let mut r = running_registry("tsk_001");
        assert_eq!(r.cancel_every_live(false), vec!["tsk_001"]);
        assert!(!r.canceled_at_deadline("tsk_001"));
    }

    /// Only the deadline spares: every other termination cancels a marked task at once.
    #[test]
    fn cancel_every_live_without_sparing_cancels_a_task_holding_a_standing_answer() {
        let mut r = running_registry("tsk_001");
        r.mark_standing_answer("tsk_001");
        assert_eq!(r.cancel_every_live(false), vec!["tsk_001"]);
        assert!(r.is_canceled("tsk_001"));
        assert!(!r.stopped_at_deadline("tsk_001"));
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Canceled)
        );
    }

    /// A cleared mark is not spared.
    #[test]
    fn cancel_every_live_at_the_deadline_cancels_a_task_whose_mark_was_cleared() {
        let mut r = running_registry("tsk_001");
        r.mark_standing_answer("tsk_001");
        r.clear_standing_answer("tsk_001");
        assert_eq!(r.cancel_every_live(true), vec!["tsk_001"]);
        assert!(r.is_canceled("tsk_001"));
        assert!(!r.stopped_at_deadline("tsk_001"));
    }

    #[test]
    fn request_cancel_on_an_unknown_id_is_unknown() {
        let mut r = make_registry();
        assert_eq!(r.request_cancel("tsk_doesnotexist"), CancelOutcome::Unknown);
        assert!(!r.is_canceled("tsk_doesnotexist"));
    }

    #[test]
    fn finish_task_does_not_overwrite_an_accepted_cancel() {
        let mut r = running_registry("tsk_001");
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::Accepted);
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Canceled),
            "the state recorded is the accepted cancel, and the caller is told so"
        );
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(
            r.finish_task(TaskState::Failed),
            None,
            "a slot that is no longer running records nothing"
        );
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
    }

    #[test]
    fn finish_task_returns_the_state_it_recorded() {
        let mut r = running_registry("tsk_001");
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Completed)
        );
        assert_eq!(make_registry().finish_task(TaskState::Failed), None);
    }

    #[test]
    fn an_input_timeout_leaves_the_task_working_and_is_read_once() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "prompt".into(), tx)
            .unwrap();
        r.record_input_timeout("tsk_001");
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Working,
            "the task's terminal state is not the wait's to record"
        );
        assert!(
            r.get_input_prompt("tsk_001").is_none(),
            "the waiter is gone"
        );
        assert!(r.take_input_timeout("tsk_001"));
        assert!(
            !r.take_input_timeout("tsk_001"),
            "a reopened attempt does not end on the same timeout"
        );
        assert!(!r.take_input_timeout("tsk_other"));
    }

    #[test]
    fn cancelling_a_queued_task_frees_its_queue_slot() {
        let mut r = TaskRegistry::new(2, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_b", "ctx_001");
        assert!(!r.can_accept(), "the queue is full");
        assert_eq!(r.request_cancel("tsk_b"), CancelOutcome::Accepted);
        assert!(r.can_accept(), "a cancelled task holds no slot");
    }

    #[test]
    fn close_to_new_work_rejects_every_submitted_task_and_nothing_else() {
        let mut r = TaskRegistry::new(4, TaskAcceptance::Queue);
        for id in ["tsk_a", "tsk_b", "tsk_c", "tsk_d"] {
            r.enqueue(id, &format!("ctx_{id}"));
        }
        r.start_task("tsk_a".to_string(), "ctx_tsk_a".to_string(), TaskLane::Bg);
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_a", "prompt".into(), tx).unwrap();
        assert_eq!(r.request_cancel("tsk_c"), CancelOutcome::Accepted);
        let pending_before = r.pending_count;

        let refused = r.close_to_new_work();

        assert_eq!(
            refused,
            vec![
                ("tsk_b".to_string(), "ctx_tsk_b".to_string()),
                ("tsk_d".to_string(), "ctx_tsk_d".to_string()),
            ]
        );
        for id in ["tsk_b", "tsk_d"] {
            assert_eq!(r.get_task(id).unwrap().status.state, TaskState::Rejected);
        }
        assert_eq!(r.pending_count, pending_before - 2);
        assert_eq!(r.pending_count, 0);
        assert_eq!(
            r.get_task("tsk_a").unwrap().status.state,
            TaskState::InputRequired,
            "a live task that is not submitted is left alone"
        );
        assert_eq!(
            r.get_task("tsk_c").unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(r.request_cancel("tsk_b"), CancelOutcome::AlreadyTerminal);
        assert!(!r.is_canceled("tsk_b"));
    }

    #[test]
    fn a_closed_registry_accepts_nothing_and_closes_once() {
        for (depth, mode) in [(1, TaskAcceptance::Single), (4, TaskAcceptance::Queue)] {
            let mut r = TaskRegistry::new(depth, mode.clone());
            assert!(r.can_accept());
            assert!(!r.is_closed());
            r.enqueue("tsk_a", "ctx_001");
            assert_eq!(r.close_to_new_work().len(), 1);
            assert!(r.is_closed());
            assert!(!r.can_accept(), "{mode:?}: a closed registry refuses work");
            assert!(r.close_to_new_work().is_empty(), "{mode:?}: closes once");
            assert!(!r.can_accept());
        }
    }

    /// `LifecycleConfig::tasks_held_at_once` is what `W-ROS-001` compares a member's callers
    /// against; this registry is what actually rejects a call. Filling a registry the way the
    /// door and the task loop do must accept exactly that many tasks, so a change to either rule
    /// fails here instead of leaving the warning wrong.
    #[test]
    fn tasks_held_at_once_matches_what_the_registry_accepts() {
        const SAFETY_CAP: usize = 64;
        for (mode, depth, expected) in [
            (TaskAcceptance::None, 1, 0),
            (TaskAcceptance::Single, 1, 1),
            (TaskAcceptance::Queue, 1, 2),
            (TaskAcceptance::Queue, 3, 4),
        ] {
            let mut r = TaskRegistry::new(depth, mode.clone());
            let mut accepted = 0;
            while r.can_accept() && accepted < SAFETY_CAP {
                let task_id = format!("tsk_{accepted}");
                r.enqueue(&task_id, "ctx_001");
                if accepted == 0 {
                    r.start_task(task_id, "ctx_001".to_string(), TaskLane::Peer);
                }
                accepted += 1;
            }
            let lifecycle = murmur_artifact::LifecycleConfig {
                task_acceptance: mode.clone(),
                queue_depth: depth,
                ..Default::default()
            };
            assert_eq!(accepted, expected, "{mode:?} at depth {depth}");
            assert_eq!(
                accepted,
                lifecycle.tasks_held_at_once(),
                "{mode:?} at depth {depth}: tasks_held_at_once drifted from the registry"
            );
        }
    }

    #[test]
    fn task_state_serializes_as_its_wire_name_and_reads_only_that() {
        for state in TaskState::ALL {
            let wire = serde_json::to_value(&state).unwrap();
            assert_eq!(wire, serde_json::json!(state.wire_name()));
            assert_eq!(
                serde_json::from_value::<TaskState>(wire).unwrap(),
                state,
                "{state:?}"
            );
            assert!(
                serde_json::from_value::<TaskState>(serde_json::json!(state.as_str())).is_err(),
                "{state:?}: murmur's word is not a wire state"
            );
        }
        for refused in [
            serde_json::json!("TASK_STATE_UNSPECIFIED"),
            serde_json::json!("TASK_STATE_AUTH_REQUIRED"),
            serde_json::json!("task_state_working"),
            serde_json::json!(1),
            serde_json::json!(null),
        ] {
            assert!(
                serde_json::from_value::<TaskState>(refused.clone()).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn task_state_as_str_is_murmurs_word() {
        let words: Vec<&str> = TaskState::ALL.iter().map(TaskState::as_str).collect();
        assert_eq!(
            words,
            [
                "submitted",
                "working",
                "completed",
                "failed",
                "canceled",
                "input-required",
                "rejected"
            ]
        );
    }

    #[test]
    fn murmur_state_name_maps_the_eight_v1_states_and_nothing_else() {
        for state in TaskState::ALL {
            assert_eq!(murmur_state_name(state.wire_name()), Some(state.as_str()));
        }
        assert_eq!(
            murmur_state_name("TASK_STATE_AUTH_REQUIRED"),
            Some("auth-required")
        );
        for other in [
            "TASK_STATE_UNSPECIFIED",
            "working",
            "",
            "TASK_STATE_working",
        ] {
            assert_eq!(murmur_state_name(other), None, "{other:?}");
        }
    }

    #[test]
    fn roles_serialize_as_their_proto_names() {
        assert_eq!(
            serde_json::to_value(Role::User).unwrap(),
            serde_json::json!("ROLE_USER")
        );
        assert_eq!(
            serde_json::to_value(Role::Agent).unwrap(),
            serde_json::json!("ROLE_AGENT")
        );
        assert!(serde_json::from_value::<Role>(serde_json::json!("agent")).is_err());
    }

    #[test]
    fn finish_task_cleans_up_input_waiters() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "prompt".into(), tx)
            .unwrap();
        r.finish_task(TaskState::Failed);
        // input_waiters should be cleaned up
        assert!(r.get_input_prompt("tsk_001").is_none());
    }

    #[test]
    fn a2a_error_table_is_the_spec_table_in_code_order() {
        let table: Vec<(i32, &str)> = A2aError::ALL
            .into_iter()
            .map(|error| (error.code(), error.reason()))
            .collect();
        assert_eq!(
            table,
            [
                (-32001, "TASK_NOT_FOUND"),
                (-32002, "TASK_NOT_CANCELABLE"),
                (-32003, "PUSH_NOTIFICATION_NOT_SUPPORTED"),
                (-32004, "UNSUPPORTED_OPERATION"),
                (-32005, "CONTENT_TYPE_NOT_SUPPORTED"),
                (-32006, "INVALID_AGENT_RESPONSE"),
                (-32007, "EXTENDED_AGENT_CARD_NOT_CONFIGURED"),
                (-32008, "EXTENSION_SUPPORT_REQUIRED"),
                (-32009, "VERSION_NOT_SUPPORTED"),
            ]
        );
    }

    #[test]
    fn a2a_error_from_code_round_trips_and_knows_no_other_code() {
        for error in A2aError::ALL {
            assert_eq!(A2aError::from_code(error.code()), Some(error));
        }
        for code in [-32000, -32010, -32099, -32600, -31001, 0] {
            assert_eq!(A2aError::from_code(code), None, "{code}");
        }
    }

    #[test]
    fn a2a_murmur_errors_lie_outside_the_reserved_range() {
        let table: Vec<(i32, &str)> = MurmurError::ALL
            .into_iter()
            .map(|error| (error.code(), error.reason()))
            .collect();
        assert_eq!(
            table,
            [
                (-31001, "COMPLETION_MISADDRESSED"),
                (-31002, "COMPLETION_NOT_AWAITED")
            ]
        );
        for error in MurmurError::ALL {
            assert!(!(-32768..=-32000).contains(&error.code()), "{error:?}");
        }
    }

    fn wire(response: &JsonRpcResponse) -> Value {
        serde_json::to_value(response).unwrap()
    }

    #[test]
    fn a2a_error_carries_one_error_info_with_string_metadata() {
        let response = JsonRpcResponse::a2a_error(
            Value::from(7),
            A2aError::TaskNotCancelable,
            "Task cannot be canceled",
            &[("taskId", "tsk_1".into()), ("state", "completed".into())],
        );
        assert_eq!(
            wire(&response),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "error": {
                    "code": -32002,
                    "message": "Task cannot be canceled",
                    "data": [{
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "TASK_NOT_CANCELABLE",
                        "domain": "a2a-protocol.org",
                        "metadata": {"taskId": "tsk_1", "state": "completed"},
                    }],
                },
            })
        );
    }

    #[test]
    fn a2a_murmur_error_is_in_the_murmur_domain() {
        let response = JsonRpcResponse::murmur_error(
            Value::from("a"),
            MurmurError::CompletionNotAwaited,
            "nobody waits",
            &[],
        );
        let error = &wire(&response)["error"];
        assert_eq!(error["code"], -31002);
        assert_eq!(error["data"][0]["reason"], "COMPLETION_NOT_AWAITED");
        assert_eq!(error["data"][0]["domain"], "murmur.nexus");
        assert_eq!(error["data"][0]["metadata"], serde_json::json!({}));
    }

    #[test]
    fn a2a_standard_errors_carry_no_data() {
        let error = &wire(&JsonRpcResponse::err(
            Value::Null,
            -32601,
            "Method not found",
        ))["error"];
        assert!(error.get("data").is_none(), "{error}");
    }

    fn refusal(body: &str) -> Value {
        wire(&JsonRpcRequest::from_body(body).expect_err(body))
    }

    #[test]
    fn a2a_from_body_reads_a_request_and_defaults_params() {
        let req = JsonRpcRequest::from_body(
            r#"{"jsonrpc":"2.0","id":"x","method":"GetExtendedAgentCard","extra":1}"#,
        )
        .unwrap();
        assert_eq!(req.id, Value::from("x"));
        assert_eq!(req.method, "GetExtendedAgentCard");
        assert_eq!(req.params, serde_json::json!({}));

        let req = JsonRpcRequest::from_body(
            r#"{"jsonrpc":"2.0","id":3,"method":"GetTask","params":[1]}"#,
        )
        .unwrap();
        assert_eq!(req.id, Value::from(3));
        assert_eq!(req.params, serde_json::json!([1]), "a method judges params");
    }

    #[test]
    fn a2a_from_body_refuses_what_is_not_json() {
        let answer = refusal("{not json");
        assert_eq!(answer["id"], Value::Null);
        assert_eq!(answer["error"]["code"], -32700);
        assert_eq!(answer["error"]["message"], "Invalid JSON payload");
    }

    #[test]
    fn a2a_from_body_refuses_what_is_not_a_json_rpc_request() {
        for (body, id) in [
            (
                r#"{"jsonrpc":"1.0","id":1,"method":"GetTask"}"#,
                Value::from(1),
            ),
            (r#"{"id":1,"method":"GetTask"}"#, Value::from(1)),
            (
                r#"{"jsonrpc":2.0,"id":1,"method":"GetTask"}"#,
                Value::from(1),
            ),
            (
                r#"[{"jsonrpc":"2.0","id":1,"method":"GetTask"}]"#,
                Value::Null,
            ),
            (r#""GetTask""#, Value::Null),
            (r#"{"jsonrpc":"2.0","id":"a","method":7}"#, Value::from("a")),
            (r#"{"jsonrpc":"2.0","id":"a"}"#, Value::from("a")),
            (
                r#"{"jsonrpc":"2.0","id":{"a":1},"method":"GetTask"}"#,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":null,"method":"GetTask"}"#,
                Value::Null,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1.5,"method":"GetTask"}"#,
                Value::Null,
            ),
            (r#"{"jsonrpc":"2.0","method":"GetTask"}"#, Value::Null),
        ] {
            let answer = refusal(body);
            assert_eq!(answer["error"]["code"], -32600, "{body}");
            assert_eq!(
                answer["error"]["message"], "Request payload validation error",
                "{body}"
            );
            assert_eq!(answer["id"], id, "{body}");
        }
    }

    fn versions(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn a2a_version_accepts_1_0_with_or_without_a_patch() {
        for value in ["1.0", " 1.0 ", "1.0.0", "1.0.1", "1.0.17"] {
            assert!(accepts_a2a_version(&versions(&[value])), "{value:?}");
        }
    }

    #[test]
    fn a2a_version_refuses_every_other_value_and_more_than_one_line() {
        for values in [
            &[][..],
            &[""],
            &["0.3"],
            &["0.3.0"],
            &["2.0"],
            &["1"],
            &["1.1"],
            &["1.0x"],
            &["1.0."],
            &["1.0.a"],
            &["1.00"],
            &["v1.0"],
            &["1.0", "1.0"],
        ] {
            assert!(!accepts_a2a_version(&versions(values)), "{values:?}");
        }
    }

    #[test]
    fn an_extension_is_activated_by_an_exact_entry_on_any_line() {
        const URI: &str =
            "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1";
        let other = "https://example.com/x/v1";
        for values in [
            vec![URI.to_string()],
            vec![format!(" {URI} ")],
            vec![format!("{other}, {URI}")],
            vec![format!("{other},,{URI},")],
            vec![other.to_string(), URI.to_string()],
        ] {
            assert!(activates_extension(&values, URI), "{values:?}");
        }
        for values in [
            vec![],
            vec![String::new()],
            vec![other.to_string()],
            vec![format!("{URI}/")],
            vec![URI.to_uppercase()],
            vec![format!("{URI} {other}")],
        ] {
            assert!(!activates_extension(&values, URI), "{values:?}");
        }
    }

    #[test]
    fn a2a_version_refusal_names_what_was_sent() {
        let answer = wire(&version_not_supported(Value::from(4), &versions(&[])));
        assert_eq!(answer["id"], 4);
        assert_eq!(answer["error"]["code"], -32009);
        let info = &answer["error"]["data"][0];
        assert_eq!(info["reason"], "VERSION_NOT_SUPPORTED");
        assert_eq!(info["domain"], "a2a-protocol.org");
        assert_eq!(
            info["metadata"],
            serde_json::json!({"requestedVersion": "", "supportedVersions": "1.0"})
        );

        let answer = wire(&version_not_supported(Value::Null, &versions(&[" 0.3 "])));
        assert_eq!(
            answer["error"]["data"][0]["metadata"]["requestedVersion"],
            "0.3"
        );
        let answer = wire(&version_not_supported(
            Value::Null,
            &versions(&["1.0", "1.0"]),
        ));
        assert_eq!(
            answer["error"]["data"][0]["metadata"]["requestedVersion"],
            "1.0, 1.0"
        );
    }

    // ── Parts and incoming messages ──────────────────────────────────────────

    #[test]
    fn a_part_serializes_its_one_content_field_and_reads_back() {
        for (part, wire) in [
            (
                Part::text("hi"),
                serde_json::json!({"text": "hi", "mediaType": "text/plain"}),
            ),
            (
                Part::data(serde_json::json!({"a": [1]})),
                serde_json::json!({"data": {"a": [1]}, "mediaType": "application/json"}),
            ),
            (
                Part::data(Value::Null),
                serde_json::json!({"data": null, "mediaType": "application/json"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&part).unwrap(), wire);
            assert_eq!(serde_json::from_value::<Part>(wire).unwrap(), part);
        }
        for refused in [
            serde_json::json!({}),
            serde_json::json!({"text": "a", "data": 1}),
            serde_json::json!({"kind": "file", "file": {"uri": "x"}}),
            serde_json::json!({"text": 7}),
            serde_json::json!("text"),
        ] {
            assert!(
                serde_json::from_value::<Part>(refused.clone()).is_err(),
                "{refused}"
            );
        }
    }

    fn params(message: Value) -> Value {
        serde_json::json!({ "message": message })
    }

    fn user(parts: Value) -> Value {
        serde_json::json!({"messageId": "msg_1", "role": "ROLE_USER", "parts": parts})
    }

    fn invalid_params(params: &Value) -> String {
        match read_send_params(params) {
            Err(MessageRefusal::InvalidParams(message)) => message,
            other => panic!("expected -32602 for {params}, got {other:?}"),
        }
    }

    #[test]
    fn read_send_params_reads_a_v1_message_in_full() {
        let message = read_send_params(&params(serde_json::json!({
            "messageId": "msg_1",
            "contextId": "ctx_1",
            "taskId": "tsk_1",
            "role": "ROLE_USER",
            "referenceTaskIds": ["tsk_a", "tsk_b", "tsk_a"],
            "parts": [{"text": "a"}, {"data": null, "filename": "f.json"}],
            "metadata": {"ignored": true},
            "extensions": ["https://example.com/x"],
            "unknown": 1,
        })))
        .unwrap();
        assert_eq!(message.message_id, "msg_1");
        assert_eq!(message.context_id.as_deref(), Some("ctx_1"));
        assert_eq!(message.task_id.as_deref(), Some("tsk_1"));
        assert_eq!(message.reference_task_ids, ["tsk_a", "tsk_b"]);
        assert_eq!(message.part_kinds(), ["text", "data"]);
        assert_eq!(message.parts[1].filename.as_deref(), Some("f.json"));

        let message = read_send_params(&params(serde_json::json!({
            "messageId": "msg_1", "role": "ROLE_USER", "taskId": "", "parts": [{"text": "a"}],
        })))
        .unwrap();
        assert_eq!(message.task_id, None, "an empty taskId names no task");
    }

    /// Each structural check refuses with `-32602` in the documented order, so a message wrong in
    /// two ways learns about the earlier one.
    #[test]
    fn read_send_params_refuses_structure_in_order() {
        let text = serde_json::json!([{"text": "a"}]);
        for (params, expected) in [
            (serde_json::json!({}), "params.message"),
            (user(text.clone()), "params.message"),
            (serde_json::json!({"message": "hi"}), "params.message"),
            (
                params(serde_json::json!({"role": "ROLE_USER", "parts": text})),
                "messageId",
            ),
            (
                params(serde_json::json!({"messageId": "", "role": "ROLE_USER", "parts": text})),
                "messageId",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "user", "parts": []})),
                "ROLE_USER",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_AGENT", "parts": text})),
                "ROLE_USER",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": 1, "parts": text})),
                "ROLE_USER",
            ),
            (
                params(serde_json::json!({"messageId": "m", "parts": text})),
                "ROLE_USER",
            ),
            (params(user(serde_json::json!([]))), "non-empty array"),
            (
                params(user(serde_json::json!({"text": "a"}))),
                "non-empty array",
            ),
            (
                params(user(serde_json::json!([{"text": "a"}, {}]))),
                "message.parts[1] sets none",
            ),
            (
                params(user(serde_json::json!([{"text": "a", "data": 1}]))),
                "message.parts[0] sets text and data",
            ),
            (
                params(user(
                    serde_json::json!([{"kind": "file", "file": {"uri": "x"}}]),
                )),
                "message.parts[0] sets none",
            ),
            (
                params(user(serde_json::json!([{"url": 7}, {}]))),
                "message.parts[1] sets none",
            ),
            (
                params(user(serde_json::json!([{"text": 7}]))),
                "message.parts[0].text is not a string",
            ),
            (
                params(user(serde_json::json!([{"data": 1, "mediaType": 2}]))),
                "message.parts[0].mediaType",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                    "parts": [{"url": "u"}], "taskId": 7})),
                "message.taskId",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                    "parts": text, "contextId": ["c"]})),
                "message.contextId",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                    "parts": [{"raw": "aGk="}], "referenceTaskIds": [""]})),
                "referenceTaskIds",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                    "parts": text, "referenceTaskIds": [7]})),
                "referenceTaskIds",
            ),
            (
                params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                    "parts": text, "referenceTaskIds": "tsk_a"})),
                "referenceTaskIds",
            ),
        ] {
            let message = invalid_params(&params);
            assert!(message.contains(expected), "{params}: {message}");
        }
    }

    #[test]
    fn read_send_params_caps_distinct_references() {
        let ids = |n: usize| -> Vec<String> { (0..n).map(|i| format!("tsk_{i}")).collect() };
        let with = |ids: Vec<String>| {
            params(serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                "parts": [{"text": "a"}], "referenceTaskIds": ids}))
        };
        let mut sixteen_twice = ids(MAX_REFERENCE_TASK_IDS);
        sixteen_twice.extend(ids(MAX_REFERENCE_TASK_IDS));
        assert_eq!(
            read_send_params(&with(sixteen_twice))
                .unwrap()
                .reference_task_ids,
            ids(MAX_REFERENCE_TASK_IDS)
        );
        let message = invalid_params(&with(ids(MAX_REFERENCE_TASK_IDS + 1)));
        assert!(message.contains("names 17 tasks"), "{message}");
    }

    /// A message asking for push notifications is refused, whatever else `configuration` holds;
    /// a `null` config asks for nothing.
    #[test]
    fn read_send_params_refuses_a_push_notification_config() {
        let message = user(serde_json::json!([{"text": "hi"}]));
        let asking = serde_json::json!({"message": message,
            "configuration": {"taskPushNotificationConfig": {"url": "https://example.com/hook"}}});
        assert_eq!(
            read_send_params(&asking),
            Err(MessageRefusal::PushNotificationNotSupported)
        );
        let answer = serde_json::to_value(
            MessageRefusal::PushNotificationNotSupported.into_response(Value::from(1)),
        )
        .unwrap();
        assert_eq!(answer["error"]["code"], -32003);
        assert_eq!(
            answer["error"]["message"],
            "push notifications are not supported by this agent"
        );
        assert_eq!(
            answer["error"]["data"][0],
            serde_json::json!({"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": "PUSH_NOTIFICATION_NOT_SUPPORTED", "domain": "a2a-protocol.org",
                "metadata": {}})
        );
        for configuration in [
            serde_json::json!({"taskPushNotificationConfig": null}),
            serde_json::json!({"acceptedOutputModes": ["text/plain"], "historyLength": 2}),
            serde_json::json!("ignored"),
        ] {
            let params = serde_json::json!({"message": message, "configuration": configuration});
            assert!(read_send_params(&params).is_ok(), "{params}");
        }
    }

    /// A message whose `metadata` is `metadata`, naming `task_id` when given.
    fn with_metadata(metadata: Value, task_id: Option<&str>) -> Value {
        let mut message = user(serde_json::json!([{"text": "a"}]));
        message["metadata"] = metadata;
        if let Some(task_id) = task_id {
            message["taskId"] = Value::from(task_id);
        }
        params(message)
    }

    /// `metadata` keyed by the door extension's URI, holding `door`.
    fn door(door: Value) -> Value {
        serde_json::json!({ DOOR_EXTENSION_URI: door })
    }

    #[test]
    fn read_send_params_reads_forget_session_true() {
        let message = read_send_params(&with_metadata(
            door(serde_json::json!({"forgetSession": true})),
            None,
        ))
        .unwrap();
        assert!(message.forget_session);
        assert!(message.door_extension);
    }

    #[test]
    fn read_send_params_reads_every_other_metadata_as_no_forget() {
        for (metadata, door_extension) in [
            (door(serde_json::json!({"forgetSession": false})), true),
            (door(serde_json::json!({})), true),
            (
                door(serde_json::json!({"other": 1, "forgetSession": false})),
                true,
            ),
            (serde_json::json!({}), false),
            (
                serde_json::json!({"https://example.com/x": {"forgetSession": true}}),
                false,
            ),
            (serde_json::json!({"forgetSession": true}), false),
            (serde_json::json!("not an object"), false),
            (
                serde_json::json!([door(serde_json::json!({"forgetSession": true}))]),
                false,
            ),
            (Value::Null, false),
        ] {
            let message = read_send_params(&with_metadata(metadata.clone(), None)).unwrap();
            assert!(!message.forget_session, "{metadata}");
            assert_eq!(message.door_extension, door_extension, "{metadata}");
        }
        let message = read_send_params(&params(user(serde_json::json!([{"text": "a"}])))).unwrap();
        assert!(
            !message.forget_session && !message.door_extension,
            "no metadata at all"
        );
    }

    #[test]
    fn read_send_params_ignores_other_keys_beside_forget_session() {
        let message = read_send_params(&with_metadata(
            door(serde_json::json!({"forgetSession": true, "later": {"x": 1}})),
            None,
        ))
        .unwrap();
        assert!(message.forget_session);
    }

    #[test]
    fn read_send_params_refuses_a_door_extension_value_that_is_not_an_object() {
        for value in [
            serde_json::json!(true),
            serde_json::json!("forget"),
            serde_json::json!([]),
        ] {
            assert_eq!(
                invalid_params(&with_metadata(door(value), None)),
                format!("message.metadata[\"{DOOR_EXTENSION_URI}\"] must be an object")
            );
        }
    }

    #[test]
    fn read_send_params_refuses_a_forget_session_that_is_not_a_boolean() {
        for value in [serde_json::json!("true"), serde_json::json!(1), Value::Null] {
            assert_eq!(
                invalid_params(&with_metadata(
                    door(serde_json::json!({"forgetSession": value})),
                    None
                )),
                format!(
                    "message.metadata[\"{DOOR_EXTENSION_URI}\"].forgetSession must be a boolean"
                )
            );
        }
    }

    #[test]
    fn read_send_params_refuses_a_forget_on_a_message_that_continues_a_task() {
        assert_eq!(
            invalid_params(&with_metadata(
                door(serde_json::json!({"forgetSession": true})),
                Some("tsk_1")
            )),
            "forgetSession starts a new conversation, so it goes on a message that starts a task, \
             not on one that continues task tsk_1"
        );
        // `false` asks for nothing, so it may ride on a continuation.
        let message = read_send_params(&with_metadata(
            door(serde_json::json!({"forgetSession": false})),
            Some("tsk_1"),
        ))
        .unwrap();
        assert_eq!(message.task_id.as_deref(), Some("tsk_1"));
        assert!(!message.forget_session);
    }

    /// The door extension's checks come after every structural check and before the file-part
    /// refusal, each refused with `-32602` in order.
    #[test]
    fn read_send_params_checks_the_forget_field_in_order() {
        // A structural fault earlier in the message is reported first.
        let mut message = with_metadata(door(Value::from(1)), None);
        message["message"]["referenceTaskIds"] = Value::from("tsk_a");
        assert!(invalid_params(&message).contains("referenceTaskIds"));
        // Not an object beats the boolean and continuation checks (they cannot be reached).
        let message = with_metadata(door(Value::from(1)), Some("tsk_1"));
        assert!(invalid_params(&message).ends_with("must be an object"));
        // Not a boolean beats the continuation check.
        let message = with_metadata(
            door(serde_json::json!({"forgetSession": "yes"})),
            Some("tsk_1"),
        );
        assert!(invalid_params(&message).ends_with("must be a boolean"));
        // A malformed forget beats a file part.
        let mut message = with_metadata(door(serde_json::json!({"forgetSession": 1})), None);
        message["message"]["parts"] = serde_json::json!([{"raw": "aGk="}]);
        assert!(invalid_params(&message).ends_with("must be a boolean"));
        // A well-formed forget on a message with a file part is refused for the part.
        let mut message = with_metadata(door(serde_json::json!({"forgetSession": true})), None);
        message["message"]["parts"] = serde_json::json!([{"raw": "aGk="}]);
        assert!(matches!(
            read_send_params(&message),
            Err(MessageRefusal::ContentTypeNotSupported { .. })
        ));
        // Each renders as `Invalid params: …`.
        let refusal = read_send_params(&with_metadata(door(Value::from(1)), None)).unwrap_err();
        let answer = wire(&refusal.into_response(Value::from(3)));
        assert_eq!(answer["error"]["code"], -32602);
        assert_eq!(
            answer["error"]["message"],
            format!("Invalid params: message.metadata[\"{DOOR_EXTENSION_URI}\"] must be an object")
        );
    }

    /// `params` for a well-formed message with `configuration` as given.
    fn configured(configuration: Value) -> Value {
        serde_json::json!({
            "message": user(serde_json::json!([{"text": "a"}])),
            "configuration": configuration,
        })
    }

    #[test]
    fn read_send_params_reads_return_immediately_true_only_for_true() {
        let immediately = read_send_params(&configured(serde_json::json!({
            "returnImmediately": true,
        })))
        .unwrap();
        assert!(immediately.return_immediately);
        for params in [
            params(user(serde_json::json!([{"text": "a"}]))),
            configured(Value::Null),
            configured(serde_json::json!({})),
            configured(serde_json::json!({"returnImmediately": null})),
            configured(serde_json::json!({"returnImmediately": false})),
            configured(serde_json::json!({"historyLength": 2})),
            configured(serde_json::json!("ignored")),
        ] {
            assert!(
                !read_send_params(&params).unwrap().return_immediately,
                "{params}"
            );
        }
    }

    #[test]
    fn read_send_params_refuses_a_return_immediately_that_is_not_a_boolean() {
        for value in [
            Value::from("yes"),
            Value::from(1),
            serde_json::json!({}),
            serde_json::json!([true]),
        ] {
            let params = configured(serde_json::json!({ "returnImmediately": value }));
            assert_eq!(
                invalid_params(&params),
                "configuration.returnImmediately must be a boolean",
                "{params}"
            );
            let answer = wire(
                &read_send_params(&params)
                    .unwrap_err()
                    .into_response(1.into()),
            );
            assert_eq!(answer["error"]["code"], -32602);
            assert_eq!(
                answer["error"]["message"],
                "Invalid params: configuration.returnImmediately must be a boolean"
            );
        }
    }

    /// The flag is the last `-32602` check: after the door extension's, before the file part and
    /// the push config.
    #[test]
    fn read_send_params_checks_return_immediately_in_order() {
        let mut message = with_metadata(door(serde_json::json!({"forgetSession": 1})), None);
        message["configuration"] = serde_json::json!({"returnImmediately": "yes"});
        assert!(invalid_params(&message).contains("forgetSession"));

        let mut message = configured(serde_json::json!({
            "returnImmediately": "yes",
            "taskPushNotificationConfig": {"url": "https://example.com/hook"},
        }));
        message["message"]["parts"] = serde_json::json!([{"raw": "aGk="}]);
        assert!(invalid_params(&message).contains("returnImmediately"));

        message["configuration"]["returnImmediately"] = Value::Bool(true);
        assert!(matches!(
            read_send_params(&message),
            Err(MessageRefusal::ContentTypeNotSupported { .. })
        ));
    }

    /// A file part is refused only once the message is otherwise well formed, naming the first.
    #[test]
    fn read_send_params_refuses_the_first_file_part_with_content_type_not_supported() {
        assert_eq!(
            read_send_params(&params(user(serde_json::json!([
                {"text": "a"},
                {"url": "https://example.com/x.pdf", "mediaType": "application/pdf"},
                {"raw": "aGk="},
            ])))),
            Err(MessageRefusal::ContentTypeNotSupported {
                part_index: 1,
                kind: "url",
                media_type: Some("application/pdf".to_string()),
            })
        );
        let refusal =
            read_send_params(&params(user(serde_json::json!([{"raw": "aGk="}])))).unwrap_err();
        let answer = wire(&refusal.into_response(Value::from(3)));
        assert_eq!(answer["error"]["code"], -32005);
        let info = &answer["error"]["data"][0];
        assert_eq!(info["reason"], "CONTENT_TYPE_NOT_SUPPORTED");
        assert_eq!(info["metadata"], serde_json::json!({"partIndex": "0"}));
        // Malformed beats unsupported.
        invalid_params(&params(user(
            serde_json::json!([{"raw": "aGk="}, {"text": 1}]),
        )));
    }

    fn rendered(parts: Value) -> String {
        read_send_params(&params(user(parts))).unwrap().agent_text()
    }

    #[test]
    fn agent_text_of_text_parts_is_the_text_joined_by_newlines() {
        assert_eq!(
            rendered(
                serde_json::json!([{"text": "one"}, {"text": "two", "mediaType": "text/markdown"}])
            ),
            "one\ntwo"
        );
    }

    #[test]
    fn agent_text_fences_a_data_part_beyond_its_longest_backtick_run() {
        assert_eq!(
            rendered(serde_json::json!([{"text": "summarise"}, {"data": {"rows": [1, 2]}}])),
            "summarise\n```data\n{\n  \"rows\": [\n    1,\n    2\n  ]\n}\n```"
        );
        let three =
            rendered(serde_json::json!([{"data": "a ``` b", "mediaType": "application/json"}]));
        assert_eq!(
            three,
            "````data media-type=application/json\n\"a ``` b\"\n````"
        );
        let five = rendered(serde_json::json!([{"data": {"x": "`````"}}]));
        assert!(five.starts_with("``````data\n"), "{five}");
        assert!(five.ends_with("\n``````"), "{five}");
        assert_eq!(
            rendered(serde_json::json!([{"data": null}])),
            "```data\nnull\n```"
        );
    }

    #[test]
    fn agent_text_sanitises_the_fence_label() {
        assert_eq!(
            rendered(serde_json::json!([{"data": 1, "mediaType": "a b=`c",
                "filename": "my\tfile.json"}])),
            "```data media-type=a_b__c filename=my_file.json\n1\n```"
        );
    }

    #[test]
    fn a_reference_block_names_each_task_as_it_stands() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        for id in ["tsk_a", "tsk_b", "tsk_c", "tsk_e"] {
            r.enqueue(id, "ctx_001");
        }
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_a", "done", Some("four ```"), false, &[], &[]);
        r.start_task("tsk_b".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r.finish_task(TaskState::Failed);
        r.record_ending("tsk_b", "the driver\nfailed", None, false, &[], &[]);
        r.start_task("tsk_c".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r.record_owner("tsk_e", "member:reviewer");
        let ids: Vec<String> = ["tsk_a", "tsk_b", "tsk_c", "tsk_d", "tsk_e"]
            .map(str::to_string)
            .to_vec();

        assert_eq!(
            r.reference_block(&ids, |_| true).unwrap(),
            "Referenced tasks:\n\
             - tsk_a: completed\n````response\nfour ```\n````\n\
             - tsk_b: failed: the driver failed\n\
             - tsk_c: working\n\
             - tsk_d: not a task this capsule holds\n\
             - tsk_e: submitted"
        );
        let member = |task_id: &str| r.owner(task_id) == Some("member:planner");
        let block = r.reference_block(&ids[4..], member).unwrap();
        assert_eq!(
            block,
            "Referenced tasks:\n- tsk_e: not a task this capsule holds"
        );
        assert_eq!(r.reference_block(&[], |_| true), None);
    }

    #[test]
    fn user_message_is_a_v1_message_from_the_user() {
        assert_eq!(
            user_message("msg_1", Some("ctx_1"), "hello"),
            serde_json::json!({
                "messageId": "msg_1",
                "contextId": "ctx_1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello", "mediaType": "text/plain"}],
            })
        );
        assert!(user_message("msg_1", None, "hello")
            .get("contextId")
            .is_none());
        assert!(read_send_params(&params(user_message("msg_1", None, "hello"))).is_ok());
    }

    #[test]
    fn send_message_params_answer_at_acceptance() {
        let message = user_message("msg_1", None, "hello");
        let params = send_message_params(message.clone());
        assert_eq!(
            params,
            serde_json::json!({
                "message": message,
                "configuration": {"returnImmediately": true},
            })
        );
        assert!(read_send_params(&params).unwrap().return_immediately);
    }

    #[test]
    fn send_message_result_is_exactly_one_member_of_the_oneof() {
        let task = serde_json::json!({"id": "tsk_1", "contextId": "c",
            "status": {"state": "TASK_STATE_SUBMITTED"}});
        assert!(matches!(
            SendMessageResult::from_result(&serde_json::json!({ "task": task })),
            Ok(SendMessageResult::Task(task)) if task.id == "tsk_1"
        ));
        assert!(matches!(
            SendMessageResult::from_result(&serde_json::json!({"message": {"messageId": "m"}})),
            Ok(SendMessageResult::Message(_))
        ));
        for refused in [
            serde_json::json!({}),
            serde_json::json!({"task": task, "message": {}}),
            task.clone(),
            serde_json::json!({"task": {"id": "tsk_1", "contextId": "c",
                "status": {"state": "submitted"}}}),
        ] {
            assert!(
                SendMessageResult::from_result(&refused).is_err(),
                "{refused}"
            );
        }
    }
}
