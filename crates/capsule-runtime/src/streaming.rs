//! The capsule's stream: the A2A v1.0 events `SendStreamingMessage` and `murmur.WatchSession`
//! write, and the murmur frames the stream extension adds to them.
//!
//! Every event is numbered, buffered and broadcast once, here, as a [`StreamEvent`] holding the
//! `StreamResponse` it renders to with and without the extension. Each connection renders it with
//! its own JSON-RPC request id ([`sse_event`]), so one buffer serves connections that differ in
//! both. The writers both transports call are [`emit_status`], [`emit_input_required`],
//! [`emit_task_ending`] and [`emit_frame`].
//!
//! A murmur frame rides in a `statusUpdate` of its task that repeats `TASK_STATE_WORKING` with no
//! message, under `metadata` keyed by [`STREAM_EXTENSION_URI`], so a v1.0 client that does not
//! know the extension reads a status it already holds. A frame that belongs to no task rides in a
//! `message`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::a2a::{A2aArtifact, A2aTask, Part, Role, StatusMessage, TaskState, TaskStatus};

/// URI of the agent-card extension that lists the murmur frames the capsule's stream can write,
/// and the `A2A-Extensions` value a client sends to receive them. It is the address of that
/// extension's section in the reference docs, on the streaming protocol page beside the frames it
/// names. Every murmur frame on the wire sits under `metadata` keyed by this URI.
pub const STREAM_EXTENSION_URI: &str =
    murmur_artifact::docs_reference_url!("streaming-protocol/#murmur-stream-v1");

pub(crate) type SseBroadcast = broadcast::Sender<Arc<StreamEvent>>;

/// Where a task's events go: the session's broadcast and its replay buffer, or `None` for a run
/// no client can watch.
pub(crate) type StreamSink = Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>;

/// Cadence at which an idle SSE connection writes [`SSE_HEARTBEAT_COMMENT`].
///
/// Shared by `SendStreamingMessage` and `murmur.WatchSession` so the two endpoints present one
/// liveness contract. Not configurable.
pub(crate) const SSE_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The liveness signal written to an idle SSE connection.
///
/// An SSE comment, not an event: it carries no `id:` line, is written straight to the socket
/// rather than through the buffer, and so consumes no event id and never enters the replay
/// buffer. A client that counts events sees none of these.
pub(crate) const SSE_HEARTBEAT_COMMENT: &[u8] = b":heartbeat\n\n";

/// The task an event belongs to: every task-bound event names both ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreamTask {
    pub task_id: String,
    pub context_id: String,
}

impl StreamTask {
    pub(crate) fn new(task_id: impl Into<String>, context_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            context_id: context_id.into(),
        }
    }

    /// The task of a run that serves an A2A task, which carries both ids, or `None` for one that
    /// serves none.
    pub(crate) fn of(task_id: Option<&str>, context_id: Option<&str>) -> Option<Self> {
        Some(Self::new(task_id?, context_id?))
    }
}

/// How a status event ends a `SendStreamingMessage` connection on its task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamEnd {
    /// The task's terminal status: the stream ends whether it was replayed or live.
    Terminal,
    /// The task stopped for input: a live one ends the stream, a replayed one does not, since the
    /// task has left that state by the time a continuation replays it.
    Interrupted,
}

/// One event in the session's sequence, rendered once for every connection.
#[derive(Debug)]
pub(crate) struct StreamEvent {
    /// Its place in the session's sequence: `1` for the first event, one higher for each after.
    pub id: u64,
    pub task_id: String,
    /// The `StreamResponse` a connection that did not activate the stream extension receives, or
    /// `None` for a murmur frame, which it does not.
    pub plain: Option<String>,
    /// The `StreamResponse` an activated connection receives.
    pub extended: String,
    /// The state of a standard `statusUpdate`, which a connection's later carriers repeat. `None`
    /// for an `artifactUpdate` and a murmur frame.
    pub state: Option<TaskState>,
    pub ends: Option<StreamEnd>,
}

impl StreamEvent {
    /// The `result` a connection receives for this event, or `None` when it receives nothing.
    pub(crate) fn result(&self, activated: bool) -> Option<&str> {
        if activated {
            Some(&self.extended)
        } else {
            self.plain.as_deref()
        }
    }
}

/// One SSE event: an `id:` line for an event in the session's sequence, then one `data:` line
/// holding a JSON-RPC response to `request_id` (already JSON) whose `result` is `result`. No
/// event has an `event:` line: a v1.0 client reads every `data:` line as a JSON-RPC response.
pub(crate) fn sse_event(id: Option<u64>, request_id: &str, result: &str) -> String {
    let id_line = id.map(|id| format!("id: {id}\n")).unwrap_or_default();
    format!("{id_line}data: {{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":{result}}}\n\n")
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamArtifact {
    pub tool_name: String,
    pub content: String,
    /// The fence source naming what produced `content` — `tool:<name>` — or `None` when
    /// `content` carries no fence. It tells apart the frame shapes that are otherwise
    /// identical on the wire: an ordinary tool result, a skill result, a dispatch failure and
    /// a hook artifact all arrive as a `toolName` and a `content`.
    ///
    /// Serialized on every frame, `null` included, without `skip_serializing_if`: a consumer
    /// that sees the key knows it is talking to a runtime that labels its frames, and one that
    /// sees no key knows it is not — which is a different thing from "this frame is unfenced".
    /// The conversation record's `fence` key is absent-when-unset instead, because a record
    /// line is read alongside four other envelope keys that all work that way.
    ///
    /// Naming a fence never justifies rewriting one: `content` is the bytes the model received,
    /// markers and all.
    pub fence_source: Option<String>,
    /// The provider's id for the call this frame reports, or `None` for a frame no call is
    /// behind (a hook artifact) and for a call the provider issued no id for. An empty provider
    /// id is `None` too, by the rule `TraceWriter::write_tool_call` applies to the same id.
    pub tool_call_id: Option<String>,
    /// Whether the call failed: the tool returned a status other than `passed`, or the dispatch
    /// never reached a tool. The call's `tool_call` or `skill_call` trace record spells the same
    /// fact as `"status":"error"`.
    pub is_error: bool,
    /// Wall-clock time of the dispatch, the same measurement the call's `tool_call` or
    /// `skill_call` trace record carries. `None` for a hook artifact.
    pub duration_ms: Option<u64>,
    /// The exit status of the subprocess this dispatch ran to completion, or `None` when it ran
    /// none — a WASM tool, a skill, a command demoted to the background, a failed dispatch, a
    /// hook artifact.
    pub exit_code: Option<i32>,
    /// The tool result's `truncated` flag, passed through unchanged: a tool's declaration that its
    /// output was cut short. The shell tool sets it when output exceeds its capture limit.
    pub truncated: bool,
}

// Every field above is serialized on every frame, `null` included, for the reason
// `fence_source` documents: an absent key tells a consumer the runtime does not report the fact,
// which is not the same as the fact not applying to this frame.
impl StreamArtifact {
    /// The frame for one dispatched tool call, whether it reached a tool or failed before one.
    // One parameter per wire field, so a call site names every value the frame carries.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn tool_call(
        tool_name: String,
        content: String,
        fence_source: Option<String>,
        tool_call_id: &str,
        is_error: bool,
        duration_ms: u64,
        exit_code: Option<i32>,
        truncated: bool,
    ) -> Self {
        Self {
            tool_name,
            content,
            fence_source,
            tool_call_id: (!tool_call_id.is_empty()).then(|| tool_call_id.to_string()),
            is_error,
            duration_ms: Some(duration_ms),
            exit_code,
            truncated,
        }
    }

    /// The frame forwarding one hook artifact. No tool call is behind it, so it has no id, no
    /// duration, no exit code and nothing that could have been truncated, and it never reports
    /// an error. A hook artifact is the capsule operator's own declared hook speaking, not
    /// content a tool fetched, so it reaches the model unfenced.
    pub(crate) fn hook(hook_name: String, payload: String) -> Self {
        Self {
            tool_name: hook_name,
            content: payload,
            fence_source: None,
            tool_call_id: None,
            is_error: false,
            duration_ms: None,
            exit_code: None,
            truncated: false,
        }
    }
}

/// A murmur frame: the name every frame carries under `frame`, first, and the vocabulary the
/// stream extension's `params.frames` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamFrame {
    Text,
    Thinking,
    ToolCallStarted,
    ToolCallProgress,
    Artifact,
    Gap,
    Lagged,
    ConnectionAck,
    CapsuleClosed,
}

impl StreamFrame {
    /// Every frame, in the order the stream extension's `params.frames` lists them.
    pub(crate) const ALL: [StreamFrame; 9] = [
        StreamFrame::Text,
        StreamFrame::Thinking,
        StreamFrame::ToolCallStarted,
        StreamFrame::ToolCallProgress,
        StreamFrame::Artifact,
        StreamFrame::Gap,
        StreamFrame::Lagged,
        StreamFrame::ConnectionAck,
        StreamFrame::CapsuleClosed,
    ];

    /// The frame's `frame` value, and its name in `params.frames`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            StreamFrame::Text => "text",
            StreamFrame::Thinking => "thinking",
            StreamFrame::ToolCallStarted => "tool-call-started",
            StreamFrame::ToolCallProgress => "tool-call-progress",
            StreamFrame::Artifact => "artifact",
            StreamFrame::Gap => "gap",
            StreamFrame::Lagged => "lagged",
            StreamFrame::ConnectionAck => "connection-ack",
            StreamFrame::CapsuleClosed => "capsule-closed",
        }
    }
}

/// One murmur frame and what it says: the object under `metadata[STREAM_EXTENSION_URI]`, `frame`
/// first and the rest in declaration order, camelCase.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "frame", rename_all = "kebab-case")]
pub(crate) enum MurmurFrame {
    /// Streamed text. `final` marks the cursor removal (empty) and the whole answer a
    /// non-streaming driver sends at once.
    Text { text: String, r#final: bool },
    /// Streamed reasoning. Always `final: false`.
    Thinking { text: String, r#final: bool },
    /// A tool call the model has begun writing: its id and name, nothing of its input, which a
    /// hook or the call gate has not yet seen.
    #[serde(rename_all = "camelCase")]
    ToolCallStarted {
        /// The id the call's answering `artifact` frame carries as its `toolCallId`.
        tool_call_id: String,
        /// The capsule's tool name, the one the answering `artifact` frame carries.
        tool_name: String,
    },
    /// How much of a started call's input the model has written: a byte count, never the bytes.
    #[serde(rename_all = "camelCase")]
    ToolCallProgress {
        tool_call_id: String,
        /// Bytes of the call's input JSON written so far. Never smaller than an earlier frame's
        /// for the same call.
        input_bytes: u64,
    },
    /// One tool call's result or one hook artifact, as the model received it.
    Artifact(StreamArtifact),
    /// The replay this connection asked for starts later than its cursor.
    #[serde(rename_all = "camelCase")]
    Gap { first_available_id: u64 },
    /// This connection lost `missed` live events, always at least 1.
    Lagged { missed: u64 },
    /// The first event of a `murmur.WatchSession` connection.
    #[serde(rename_all = "camelCase")]
    ConnectionAck {
        role: &'static str,
        conversation_mode: String,
    },
    /// The session's event stream closed while the door still serves the connection.
    CapsuleClosed,
}

impl MurmurFrame {
    pub(crate) fn kind(&self) -> StreamFrame {
        match self {
            MurmurFrame::Text { .. } => StreamFrame::Text,
            MurmurFrame::Thinking { .. } => StreamFrame::Thinking,
            MurmurFrame::ToolCallStarted { .. } => StreamFrame::ToolCallStarted,
            MurmurFrame::ToolCallProgress { .. } => StreamFrame::ToolCallProgress,
            MurmurFrame::Artifact(_) => StreamFrame::Artifact,
            MurmurFrame::Gap { .. } => StreamFrame::Gap,
            MurmurFrame::Lagged { .. } => StreamFrame::Lagged,
            MurmurFrame::ConnectionAck { .. } => StreamFrame::ConnectionAck,
            MurmurFrame::CapsuleClosed => StreamFrame::CapsuleClosed,
        }
    }
}

/// What the stream extension says on one event: a frame, or the reopen boundary's annotation.
#[derive(Serialize)]
#[serde(untagged)]
enum Extension<'a> {
    Frame(&'a MurmurFrame),
    Reopen { reopen: u32 },
}

/// `metadata` holding one extension payload under [`STREAM_EXTENSION_URI`].
fn extension_metadata(extension: Extension<'_>) -> BTreeMap<&'static str, Extension<'_>> {
    BTreeMap::from([(STREAM_EXTENSION_URI, extension)])
}

/// The one member of a `StreamResponse`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum StreamResponse<'a> {
    Task(&'a A2aTask),
    Message(CarrierMessage<'a>),
    StatusUpdate(StatusUpdate<'a>),
    ArtifactUpdate(ArtifactUpdate<'a>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusUpdate<'a> {
    task_id: &'a str,
    context_id: &'a str,
    status: &'a TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<BTreeMap<&'static str, Extension<'a>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactUpdate<'a> {
    task_id: &'a str,
    context_id: &'a str,
    artifact: &'a A2aArtifact,
    last_chunk: bool,
}

/// A frame that belongs to no task, carried as an agent `message` with no `contextId`: the proto
/// requires one part, so its one text part is the frame's name.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CarrierMessage<'a> {
    message_id: String,
    role: Role,
    parts: Vec<Part>,
    metadata: BTreeMap<&'static str, Extension<'a>>,
}

fn render(response: StreamResponse<'_>) -> String {
    serde_json::to_string(&response).expect("a StreamResponse always serializes")
}

/// A fresh `messageId`, unique within the session.
fn message_id() -> String {
    format!("msg_{}", uuid::Uuid::now_v7().simple())
}

/// The `task` member: how a `SendStreamingMessage` stream opens, and the whole of a refused one.
pub(crate) fn task_result(task: &A2aTask) -> String {
    render(StreamResponse::Task(task))
}

/// `frame` carried by a `message`, for a frame that belongs to no task.
pub(crate) fn message_carrier(frame: &MurmurFrame) -> String {
    render(StreamResponse::Message(CarrierMessage {
        message_id: message_id(),
        role: Role::Agent,
        parts: vec![Part::text(frame.kind().wire_name())],
        metadata: extension_metadata(Extension::Frame(frame)),
    }))
}

/// `frame` carried by a `statusUpdate` of `task` repeating `state` with no message, recorded as
/// it is written.
pub(crate) fn status_carrier(task: &StreamTask, state: TaskState, frame: &MurmurFrame) -> String {
    render(StreamResponse::StatusUpdate(StatusUpdate {
        task_id: &task.task_id,
        context_id: &task.context_id,
        status: &TaskStatus::of(state),
        metadata: Some(extension_metadata(Extension::Frame(frame))),
    }))
}

fn status_update(task: &StreamTask, status: &TaskStatus, reopen: Option<u32>) -> String {
    render(StreamResponse::StatusUpdate(StatusUpdate {
        task_id: &task.task_id,
        context_id: &task.context_id,
        status,
        metadata: reopen.map(|reopen| extension_metadata(Extension::Reopen { reopen })),
    }))
}

/// A `statusUpdate` reporting `status`, and the stream end it is.
fn status_event(task: &StreamTask, status: TaskStatus, reopen: Option<u32>) -> Draft {
    let ends = if status.state.is_terminal() {
        Some(StreamEnd::Terminal)
    } else if status.state == TaskState::InputRequired {
        Some(StreamEnd::Interrupted)
    } else {
        None
    };
    Draft {
        task_id: task.task_id.clone(),
        plain: Some(status_update(task, &status, None)),
        extended: status_update(task, &status, reopen),
        state: Some(status.state),
        ends,
    }
}

/// The `artifactUpdate` delivering `artifact` whole.
pub(crate) fn artifact_update(task: &StreamTask, artifact: &A2aArtifact) -> String {
    render(StreamResponse::ArtifactUpdate(ArtifactUpdate {
        task_id: &task.task_id,
        context_id: &task.context_id,
        artifact,
        last_chunk: true,
    }))
}

/// An event before the buffer gives it its id.
struct Draft {
    task_id: String,
    plain: Option<String>,
    extended: String,
    state: Option<TaskState>,
    ends: Option<StreamEnd>,
}

/// Result of a replay request from `SseEventBuffer::replay_from`.
pub(crate) enum ReplayResult {
    Complete(Vec<Arc<StreamEvent>>),
    /// Events after the cursor are no longer buffered, or the cursor was never issued by this
    /// session. `events` is the whole buffer; callers write a gap before it.
    WithGap {
        first_available_id: u64,
        events: Vec<Arc<StreamEvent>>,
    },
}

/// The session's event-id sequence and a bounded ring-buffer of the most recent events for
/// reconnect replay.
///
/// One buffer serves a whole capsule session, so every numbered event of every task takes its id
/// from `next_id`: the first event is `1` and each later one is exactly one higher.
pub(crate) struct SseEventBuffer {
    events: VecDeque<Arc<StreamEvent>>,
    capacity: usize,
    /// Starts at `1`, not `0`: replay writes events with an id strictly greater than the
    /// client's `Last-Event-ID`, and `0` is the cursor for "from the start", so an event numbered
    /// `0` could never be replayed.
    next_id: u64,
}

impl SseEventBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::new(),
            capacity,
            next_id: 1,
        }
    }

    /// Number and store one event.
    fn append(&mut self, draft: Draft) -> Arc<StreamEvent> {
        let id = self.next_id;
        self.next_id += 1;
        let event = Arc::new(StreamEvent {
            id,
            task_id: draft.task_id,
            plain: draft.plain,
            extended: draft.extended,
            state: draft.state,
            ends: draft.ends,
        });
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(Arc::clone(&event));
        event
    }

    /// Return the events written after `last_id`, distinguishing a clean replay from one that
    /// cannot be: events after `last_id` were evicted, or `last_id` was never issued by this
    /// session. Either of those returns the whole buffer as [`ReplayResult::WithGap`].
    pub fn replay_from(&self, last_id: u64) -> ReplayResult {
        let Some(oldest_id) = self.events.front().map(|event| event.id) else {
            return ReplayResult::Complete(vec![]);
        };

        if last_id >= self.next_id || last_id < oldest_id - 1 {
            let events = self.events.iter().cloned().collect();
            return ReplayResult::WithGap {
                first_available_id: oldest_id,
                events,
            };
        }

        let events = self
            .events
            .iter()
            .filter(|event| event.id > last_id)
            .cloned()
            .collect();
        ReplayResult::Complete(events)
    }
}

/// Give each draft the session's next id, buffer it and broadcast it, in order.
///
/// The buffer lock is held across the broadcast send, so events emitted concurrently — by the
/// agent loop, a tool's chunk host function and a `request-input` wait — enter the buffer and
/// reach every live receiver in id order, and the drafts of one call take consecutive ids. A send
/// with no receivers is not an error.
fn publish(sink: &StreamSink, drafts: impl IntoIterator<Item = Draft>) {
    let Some((tx, buf)) = sink else {
        return;
    };
    let mut buf = buf
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for draft in drafts {
        let event = buf.append(draft);
        let _ = tx.send(event);
    }
}

/// A non-terminal status of `task`: `working` with what the runtime is doing, or
/// `input-required` with the prompt. `reopen` is the 1-based ordinal of the reopen the status
/// announces, on the `working` status written between two attempts of one task; an activated
/// connection reads it as `metadata[STREAM_EXTENSION_URI].reopen`.
pub(crate) fn emit_status(
    sink: &StreamSink,
    task: &StreamTask,
    state: TaskState,
    message: &str,
    reopen: Option<u32>,
) {
    publish(
        sink,
        [status_event(
            task,
            agent_status(task, state, message),
            reopen,
        )],
    );
}

/// `task` stopped for input: an `input-required` status carrying `prompt`.
pub(crate) fn emit_input_required(sink: &StreamSink, task: &StreamTask, prompt: &str) {
    emit_status(sink, task, TaskState::InputRequired, prompt, None);
}

/// The status [`emit_input_required`] writes, as a connection writes it outside the session's
/// sequence.
pub(crate) fn input_required_result(task: &StreamTask, prompt: &str) -> String {
    status_update(
        task,
        &agent_status(task, TaskState::InputRequired, prompt),
        None,
    )
}

/// A status in `state` whose message, from the agent, is `text`, recorded now.
fn agent_status(task: &StreamTask, state: TaskState, text: &str) -> TaskStatus {
    TaskStatus::at(
        state,
        Some(StatusMessage {
            message_id: message_id(),
            context_id: task.context_id.clone(),
            task_id: task.task_id.clone(),
            role: Role::Agent,
            parts: vec![Part::text(text)],
        }),
        crate::a2a::status_now(),
    )
}

/// How `task` ended, exactly as `GetTask` reports it: the `response` artifact, when it has one,
/// as an `artifactUpdate`, then its terminal status. The two take consecutive ids.
pub(crate) fn emit_task_ending(sink: &StreamSink, task: &A2aTask) {
    publish(sink, ending_drafts(task));
}

fn ending_drafts(task: &A2aTask) -> Vec<Draft> {
    let stream_task = StreamTask::new(&task.id, &task.context_id);
    let mut drafts = Vec::new();
    if let Some(response) = task
        .artifact(crate::a2a::RESPONSE_ARTIFACT)
        .filter(|_| task.status.state == TaskState::Completed)
    {
        let update = artifact_update(&stream_task, response);
        drafts.push(Draft {
            task_id: task.id.clone(),
            plain: Some(update.clone()),
            extended: update,
            state: None,
            ends: None,
        });
    }
    drafts.push(status_event(&stream_task, task.status.clone(), None));
    drafts
}

/// The events a connection that lost `task`'s ending writes in its place, outside the session's
/// sequence: what [`emit_task_ending`] writes for the same task, rendered for a connection that
/// did not activate the extension (the two renderings are equal).
pub(crate) fn ending_results(task: &A2aTask) -> Vec<String> {
    ending_drafts(task)
        .into_iter()
        .map(|draft| draft.extended)
        .collect()
}

/// One murmur frame of `task`, which is working: a carrier only an activated connection receives.
pub(crate) fn emit_frame(sink: &StreamSink, task: &StreamTask, frame: MurmurFrame) {
    publish(
        sink,
        [Draft {
            task_id: task.task_id.clone(),
            plain: None,
            extended: status_carrier(task, TaskState::Working, &frame),
            state: None,
            ends: None,
        }],
    );
}

/// One streamed text fragment of `task`.
pub(crate) fn emit_text(sink: &StreamSink, task: &StreamTask, text: &str, r#final: bool) {
    emit_frame(
        sink,
        task,
        MurmurFrame::Text {
            text: text.to_string(),
            r#final,
        },
    );
}

/// One streamed reasoning fragment of `task`.
pub(crate) fn emit_thinking(sink: &StreamSink, task: &StreamTask, text: &str) {
    emit_frame(
        sink,
        task,
        MurmurFrame::Thinking {
            text: text.to_string(),
            r#final: false,
        },
    );
}

/// The extended `result` of every event the buffer holds, in order, for tests reading what a task
/// wrote.
#[cfg(test)]
pub(crate) fn buffered(buffer: &Mutex<SseEventBuffer>) -> Vec<serde_json::Value> {
    let events = match buffer.lock().unwrap().replay_from(0) {
        ReplayResult::Complete(events) | ReplayResult::WithGap { events, .. } => events,
    };
    events
        .iter()
        .map(|event| serde_json::from_str(&event.extended).unwrap())
        .collect()
}

/// What a `StreamResponse` is: its murmur frame's name, or its member's (`task`, `message`,
/// `status-update`, `artifact-update`).
#[cfg(test)]
pub(crate) fn event_kind(result: &serde_json::Value) -> String {
    let (member, body) = result
        .as_object()
        .and_then(|object| object.iter().next())
        .expect("a StreamResponse has one member");
    if let Some(frame) = body["metadata"][STREAM_EXTENSION_URI]["frame"].as_str() {
        return frame.to_string();
    }
    match member.as_str() {
        "statusUpdate" => "status-update".to_string(),
        "artifactUpdate" => "artifact-update".to_string(),
        other => other.to_string(),
    }
}

/// The murmur frame `result` carries, or `null`.
#[cfg(test)]
pub(crate) fn frame_of(result: &serde_json::Value) -> serde_json::Value {
    let body = result.as_object().and_then(|object| object.values().next());
    body.map(|body| body["metadata"][STREAM_EXTENSION_URI].clone())
        .filter(|frame| frame.get("frame").is_some())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn task() -> StreamTask {
        StreamTask::new("tsk_1", "ctx_1")
    }

    fn payload(frame: &MurmurFrame) -> String {
        serde_json::to_string(frame).unwrap()
    }

    fn sink(capacity: usize) -> (StreamSink, broadcast::Receiver<Arc<StreamEvent>>) {
        let (tx, rx) = broadcast::channel(capacity);
        let buf = Arc::new(Mutex::new(SseEventBuffer::new(capacity)));
        (Some((tx, buf)), rx)
    }

    fn buffer_of(sink: &StreamSink) -> &Arc<Mutex<SseEventBuffer>> {
        &sink.as_ref().unwrap().1
    }

    #[test]
    fn every_frame_payload_puts_frame_first_then_its_keys_in_order() {
        let cases = [
            (
                MurmurFrame::Text {
                    text: "Hel".into(),
                    r#final: false,
                },
                r#"{"frame":"text","text":"Hel","final":false}"#,
            ),
            (
                MurmurFrame::Thinking {
                    text: "hmm".into(),
                    r#final: false,
                },
                r#"{"frame":"thinking","text":"hmm","final":false}"#,
            ),
            (
                MurmurFrame::ToolCallStarted {
                    tool_call_id: "call_1".into(),
                    tool_name: "write_file".into(),
                },
                r#"{"frame":"tool-call-started","toolCallId":"call_1","toolName":"write_file"}"#,
            ),
            (
                MurmurFrame::ToolCallProgress {
                    tool_call_id: "call_1".into(),
                    input_bytes: 3172,
                },
                r#"{"frame":"tool-call-progress","toolCallId":"call_1","inputBytes":3172}"#,
            ),
            (
                MurmurFrame::Gap {
                    first_available_id: 42,
                },
                r#"{"frame":"gap","firstAvailableId":42}"#,
            ),
            (
                MurmurFrame::Lagged { missed: 7 },
                r#"{"frame":"lagged","missed":7}"#,
            ),
            (
                MurmurFrame::ConnectionAck {
                    role: "observer",
                    conversation_mode: "stateless".into(),
                },
                r#"{"frame":"connection-ack","role":"observer","conversationMode":"stateless"}"#,
            ),
            (MurmurFrame::CapsuleClosed, r#"{"frame":"capsule-closed"}"#),
        ];
        for (frame, expected) in cases {
            assert_eq!(payload(&frame), expected);
            assert!(expected.contains(&format!("\"frame\":\"{}\"", frame.kind().wire_name())));
        }
    }

    #[test]
    fn tool_call_frame_serializes_every_key_in_declaration_order() {
        let json = payload(&MurmurFrame::Artifact(StreamArtifact::tool_call(
            "bash".into(),
            "hello".into(),
            Some("tool:bash".into()),
            "call_1",
            false,
            12,
            Some(0),
            false,
        )));
        assert_eq!(
            json,
            r#"{"frame":"artifact","toolName":"bash","content":"hello","fenceSource":"tool:bash","toolCallId":"call_1","isError":false,"durationMs":12,"exitCode":0,"truncated":false}"#
        );
    }

    fn artifact_json(artifact: StreamArtifact) -> String {
        payload(&MurmurFrame::Artifact(artifact))
    }

    #[test]
    fn tool_call_frame_passes_truncated_through() {
        let json = artifact_json(StreamArtifact::tool_call(
            "read".into(),
            "partial".into(),
            None,
            "call_1",
            false,
            3,
            None,
            true,
        ));
        assert!(json.contains(r#""truncated":true"#), "{json}");
    }

    #[test]
    fn tool_call_frame_keeps_the_exit_code_key_with_or_without_a_subprocess() {
        let ran = artifact_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "call_1",
            true,
            1204,
            Some(2),
            false,
        ));
        assert!(ran.contains(r#""exitCode":2"#), "{ran}");
        assert!(ran.contains(r#""isError":true"#), "{ran}");

        let none = artifact_json(StreamArtifact::tool_call(
            "write-file".into(),
            String::new(),
            None,
            "call_1",
            true,
            3,
            None,
            false,
        ));
        assert!(none.contains(r#""exitCode":null"#), "{none}");
    }

    #[test]
    fn tool_call_frame_records_an_empty_provider_id_as_null() {
        let empty = artifact_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "",
            false,
            1,
            None,
            false,
        ));
        assert!(empty.contains(r#""toolCallId":null"#), "{empty}");

        let issued = artifact_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "toolu_01",
            false,
            1,
            None,
            false,
        ));
        assert!(issued.contains(r#""toolCallId":"toolu_01""#), "{issued}");
    }

    #[test]
    fn hook_frame_reports_no_call_behind_it() {
        assert_eq!(
            artifact_json(StreamArtifact::hook(
                "my-hook".into(),
                r#"{"reviewed":true}"#.into(),
            )),
            r#"{"frame":"artifact","toolName":"my-hook","content":"{\"reviewed\":true}","fenceSource":null,"toolCallId":null,"isError":false,"durationMs":null,"exitCode":null,"truncated":false}"#
        );
    }

    #[test]
    fn a_task_frame_is_a_working_status_update_carrying_it_under_the_uri() {
        let carrier: Value = serde_json::from_str(&status_carrier(
            &task(),
            TaskState::Working,
            &MurmurFrame::Text {
                text: "Hel".into(),
                r#final: false,
            },
        ))
        .unwrap();
        // The carrier is stamped as it is written.
        let status = &carrier["statusUpdate"]["status"];
        crate::a2a::assert_bare_status(status, "TASK_STATE_WORKING");
        assert_eq!(
            carrier,
            json!({"statusUpdate": {
                "taskId": "tsk_1",
                "contextId": "ctx_1",
                "status": {"state": "TASK_STATE_WORKING", "timestamp": status["timestamp"]},
                "metadata": {STREAM_EXTENSION_URI: {"frame": "text", "text": "Hel", "final": false}},
            }})
        );
    }

    #[test]
    fn a_frame_of_no_task_is_an_agent_message_named_after_it() {
        let carrier: Value =
            serde_json::from_str(&message_carrier(&MurmurFrame::CapsuleClosed)).unwrap();
        let message = &carrier["message"];
        let id = message["messageId"].as_str().unwrap();
        assert!(id.starts_with("msg_") && id.len() == 36, "{id}");
        assert_eq!(message["role"], "ROLE_AGENT");
        assert_eq!(
            message["parts"],
            json!([{"text": "capsule-closed", "mediaType": "text/plain"}])
        );
        assert_eq!(
            message["metadata"],
            json!({STREAM_EXTENSION_URI: {"frame": "capsule-closed"}})
        );
        assert!(message.get("contextId").is_none());
    }

    #[test]
    fn sse_event_is_a_json_rpc_response_with_no_event_line() {
        assert_eq!(
            sse_event(Some(3), "\"req-7\"", "{\"task\":{}}"),
            "id: 3\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"req-7\",\"result\":{\"task\":{}}}\n\n"
        );
        assert_eq!(
            sse_event(None, "9", "{}"),
            "data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\n"
        );
    }

    #[test]
    fn a_status_message_names_its_task_and_a_fresh_message_id() {
        let (sink, _rx) = sink(8);
        emit_status(&sink, &task(), TaskState::Working, "inference turn 1", None);
        emit_status(&sink, &task(), TaskState::Working, "inference turn 2", None);
        let events = buffered(buffer_of(&sink));
        let status = &events[0]["statusUpdate"]["status"];
        assert_eq!(status["state"], "TASK_STATE_WORKING");
        assert_written_now(status);
        let message = &status["message"];
        assert_eq!(message["taskId"], "tsk_1");
        assert_eq!(message["contextId"], "ctx_1");
        assert_eq!(message["role"], "ROLE_AGENT");
        assert_eq!(
            message["parts"],
            json!([{"text": "inference turn 1", "mediaType": "text/plain"}])
        );
        assert_ne!(
            message["messageId"],
            events[1]["statusUpdate"]["status"]["message"]["messageId"]
        );
        assert!(events[0]["statusUpdate"].get("metadata").is_none());
    }

    #[test]
    fn only_the_reopen_boundary_is_annotated_and_only_for_an_activated_connection() {
        let (sink, _rx) = sink(8);
        emit_status(
            &sink,
            &task(),
            TaskState::Working,
            "reopened by hook h",
            Some(2),
        );
        let buffer = buffer_of(&sink).lock().unwrap();
        let ReplayResult::Complete(events) = buffer.replay_from(0) else {
            panic!("gap");
        };
        let plain: Value = serde_json::from_str(events[0].result(false).unwrap()).unwrap();
        let extended: Value = serde_json::from_str(events[0].result(true).unwrap()).unwrap();
        assert!(plain["statusUpdate"].get("metadata").is_none());
        assert_eq!(
            extended["statusUpdate"]["metadata"],
            json!({STREAM_EXTENSION_URI: {"reopen": 2}})
        );
        assert_eq!(
            plain["statusUpdate"]["status"],
            extended["statusUpdate"]["status"]
        );
    }

    #[test]
    fn a_murmur_frame_reaches_only_an_activated_connection() {
        let (sink, _rx) = sink(8);
        emit_text(&sink, &task(), "hi", false);
        let buffer = buffer_of(&sink).lock().unwrap();
        let ReplayResult::Complete(events) = buffer.replay_from(0) else {
            panic!("gap");
        };
        assert_eq!(events[0].result(false), None);
        assert!(events[0].result(true).is_some());
        assert_eq!(events[0].ends, None);
        assert_eq!(events[0].state, None);
    }

    /// When [`ended`]'s task last changed, as the registry would have stamped it.
    const ENDED_AT: &str = "2026-10-13T09:00:00.123Z";

    fn ended(state: TaskState, response: Option<&str>) -> A2aTask {
        A2aTask {
            id: "tsk_1".into(),
            context_id: "ctx_1".into(),
            status: TaskStatus::at(
                state,
                Some(StatusMessage::agent("tsk_1", "ctx_1", "session ended")),
                ENDED_AT.parse().unwrap(),
            ),
            artifacts: response.map(|response| {
                vec![A2aArtifact::named(
                    crate::a2a::RESPONSE_ARTIFACT,
                    vec![Part::text(response)],
                )]
            }),
            metadata: None,
        }
    }

    #[test]
    fn a_completed_ending_is_its_response_then_its_status_with_consecutive_ids() {
        let (sink, _rx) = sink(8);
        emit_status(&sink, &task(), TaskState::Working, "inference turn 1", None);
        emit_task_ending(&sink, &ended(TaskState::Completed, Some("four")));
        let buffer = buffer_of(&sink).lock().unwrap();
        let ReplayResult::Complete(events) = buffer.replay_from(0) else {
            panic!("gap");
        };
        assert_eq!(
            events.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let artifact: Value = serde_json::from_str(events[1].result(false).unwrap()).unwrap();
        assert_eq!(
            artifact,
            json!({"artifactUpdate": {
                "taskId": "tsk_1",
                "contextId": "ctx_1",
                "artifact": {
                    "artifactId": "response",
                    "name": "response",
                    "parts": [{"text": "four", "mediaType": "text/plain"}],
                },
                "lastChunk": true,
            }})
        );
        assert_eq!(events[1].ends, None);
        let status: Value = serde_json::from_str(events[2].result(false).unwrap()).unwrap();
        assert_eq!(
            status["statusUpdate"]["status"],
            serde_json::to_value(&ended(TaskState::Completed, Some("four")).status).unwrap()
        );
        // The terminal status is the ended task's, its registry timestamp included.
        assert_eq!(status["statusUpdate"]["status"]["timestamp"], ENDED_AT);
        assert_eq!(events[2].ends, Some(StreamEnd::Terminal));
        assert_eq!(events[2].state, Some(TaskState::Completed));
    }

    #[test]
    fn an_ending_without_a_response_is_its_status_alone() {
        for state in [TaskState::Failed, TaskState::Canceled, TaskState::Rejected] {
            let results = ending_results(&ended(state.clone(), None));
            assert_eq!(results.len(), 1, "{state:?}");
            assert!(results[0].starts_with("{\"statusUpdate\""));
        }
        assert_eq!(ending_results(&ended(TaskState::Completed, None)).len(), 1);
    }

    #[test]
    fn input_required_interrupts_with_the_prompt_as_its_message() {
        let (sink, _rx) = sink(8);
        emit_input_required(&sink, &task(), "Which file?");
        let buffer = buffer_of(&sink).lock().unwrap();
        let ReplayResult::Complete(events) = buffer.replay_from(0) else {
            panic!("gap");
        };
        assert_eq!(events[0].ends, Some(StreamEnd::Interrupted));
        let status: Value = serde_json::from_str(events[0].result(false).unwrap()).unwrap();
        assert_eq!(
            status["statusUpdate"]["status"]["state"],
            "TASK_STATE_INPUT_REQUIRED"
        );
        assert_eq!(
            status["statusUpdate"]["status"]["message"]["parts"][0]["text"],
            "Which file?"
        );
        assert_written_now(&status["statusUpdate"]["status"]);
        let direct: Value =
            serde_json::from_str(&input_required_result(&task(), "Which file?")).unwrap();
        assert_written_now(&direct["statusUpdate"]["status"]);
    }

    /// `status` carries a timestamp in the form a status writes, of an instant no later than now.
    fn assert_written_now(status: &Value) {
        let at = crate::a2a::assert_status_timestamp(status);
        assert!(at <= crate::a2a::status_now(), "{status}");
    }

    /// A buffer of `capacity` holding `count` events, ids `1..=count`.
    fn buffer_with(capacity: usize, count: u64) -> SseEventBuffer {
        let mut buf = SseEventBuffer::new(capacity);
        for n in 1..=count {
            buf.append(Draft {
                task_id: "t".into(),
                plain: None,
                extended: n.to_string(),
                state: None,
                ends: None,
            });
        }
        buf
    }

    fn ids(events: &[Arc<StreamEvent>]) -> Vec<u64> {
        events.iter().map(|e| e.id).collect()
    }

    fn complete(result: ReplayResult) -> Vec<u64> {
        match result {
            ReplayResult::Complete(events) => ids(&events),
            ReplayResult::WithGap { .. } => panic!("expected Complete"),
        }
    }

    fn with_gap(result: ReplayResult) -> (u64, Vec<u64>) {
        match result {
            ReplayResult::WithGap {
                first_available_id,
                events,
            } => (first_available_id, ids(&events)),
            ReplayResult::Complete(_) => panic!("expected WithGap"),
        }
    }

    #[test]
    fn ids_start_at_one_and_rise_by_one() {
        let (sink, mut rx) = sink(8);
        emit_status(&sink, &task(), TaskState::Working, "inference turn 1", None);
        emit_text(&sink, &task(), "a", false);
        emit_thinking(&sink, &task(), "b");
        emit_text(&sink, &task(), "", true);
        emit_task_ending(&sink, &ended(TaskState::Failed, None));
        let received: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| e.id)
            .collect();
        assert_eq!(received, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn replay_after_eviction_writes_a_gap() {
        let buf = buffer_with(3, 5);
        assert_eq!(with_gap(buf.replay_from(1)), (3, vec![3, 4, 5]));
        assert_eq!(with_gap(buf.replay_from(0)), (3, vec![3, 4, 5]));
        assert_eq!(complete(buf.replay_from(2)), vec![3, 4, 5]);
    }

    #[test]
    fn replay_from_zero_without_eviction_is_every_event() {
        let buf = buffer_with(8, 5);
        assert_eq!(complete(buf.replay_from(0)), vec![1, 2, 3, 4, 5]);
        assert_eq!(complete(buf.replay_from(3)), vec![4, 5]);
        assert_eq!(complete(buf.replay_from(5)), Vec::<u64>::new());
    }

    #[test]
    fn replay_from_a_cursor_never_issued_writes_a_gap() {
        assert_eq!(
            with_gap(buffer_with(8, 5).replay_from(99)),
            (1, vec![1, 2, 3, 4, 5])
        );
        assert_eq!(
            with_gap(buffer_with(3, 5).replay_from(6)),
            (3, vec![3, 4, 5])
        );
    }

    #[test]
    fn replay_from_an_empty_buffer_is_empty() {
        let buf = SseEventBuffer::new(8);
        assert_eq!(complete(buf.replay_from(0)), Vec::<u64>::new());
        assert_eq!(complete(buf.replay_from(99)), Vec::<u64>::new());
    }

    #[test]
    fn concurrent_emitters_share_one_ascending_sequence() {
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 200;
        let total = THREADS * PER_THREAD;
        let (sink, mut rx) = sink(total as usize);
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let sink = sink.clone();
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        if t % 2 == 0 {
                            emit_text(&sink, &task(), "x", false);
                        } else {
                            emit_status(&sink, &task(), TaskState::Working, "w", None);
                        }
                    }
                });
            }
        });

        let buffered = complete(buffer_of(&sink).lock().unwrap().replay_from(0));
        let received: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| e.id)
            .collect();
        let expected: Vec<u64> = (1..=total).collect();
        assert_eq!(buffered, expected, "buffer order is id order");
        assert_eq!(received, expected, "live delivery order is id order");
    }

    #[test]
    fn event_kind_names_the_frame_or_the_member() {
        let text = status_carrier(
            &task(),
            TaskState::Working,
            &MurmurFrame::Text {
                text: String::new(),
                r#final: true,
            },
        );
        assert_eq!(event_kind(&serde_json::from_str(&text).unwrap()), "text");
        let status = status_update(&task(), &TaskStatus::of(TaskState::Working), None);
        assert_eq!(
            event_kind(&serde_json::from_str(&status).unwrap()),
            "status-update"
        );
    }

    /// One non-comment production line of a file under `src/`, other than a `*_tests.rs` file.
    struct CodeLine {
        /// The path relative to `src/`.
        file: String,
        /// The `fn`, or the `impl` header, the line most recently entered.
        item: String,
        number: usize,
        code: String,
    }

    fn production_code() -> Vec<CodeLine> {
        let mut out = Vec::new();
        for (file, source) in crate::source_scan::crate_sources() {
            if file.ends_with("_tests.rs") {
                continue;
            }
            let mut item = String::new();
            for (index, line) in crate::source_scan::production_part(&source)
                .lines()
                .enumerate()
            {
                let code = line.trim();
                if code.starts_with("//") {
                    continue;
                }
                if let Some((_, rest)) = code.split_once("fn ") {
                    item = rest.split('(').next().unwrap_or(rest).to_string();
                } else if code.starts_with("impl ") {
                    item = code.trim_end_matches('{').trim().to_string();
                }
                out.push(CodeLine {
                    file: file.clone(),
                    item: item.clone(),
                    number: index + 1,
                    code: code.to_string(),
                });
            }
        }
        out
    }

    /// No event a connection writes has an `event:` line: a v1.0 client reads every `data:` line
    /// as a JSON-RPC response, and a browser `EventSource` delivers only unnamed events to
    /// `onmessage`.
    #[test]
    fn stream_frame_no_writer_names_an_event() {
        let named: Vec<String> = production_code()
            .into_iter()
            .filter(|line| line.code.contains("\"event: ") || line.code.contains("\\nevent: "))
            .map(|line| format!("{}:{}: {}", line.file, line.number, line.code))
            .collect();
        assert!(
            named.is_empty(),
            "lines writing an event: line:\n{}",
            named.join("\n")
        );
    }

    /// A frame written straight to one streaming connection must be one `writes_frame` lets that
    /// method carry, or the card of a door that serves only the other method omits a frame it
    /// sends. Any other write goes to the session's broadcast, which both methods relay.
    #[test]
    fn stream_frame_every_write_is_carried_by_its_method() {
        use crate::identity::{writes_frame, DoorMethod, TransportKind};
        let mut uncarried = Vec::new();
        for line in production_code() {
            let methods: &[DoorMethod] = match (line.file.as_str(), line.item.as_str()) {
                ("streaming.rs", _) | ("identity.rs", "writes_frame") => continue,
                ("identity.rs", "handle_message_stream") => &[DoorMethod::SendStreamingMessage],
                ("identity.rs", "handle_watch_session") => &[DoorMethod::WatchSession],
                _ => &[DoorMethod::SendStreamingMessage, DoorMethod::WatchSession],
            };
            let named = line.code.split("MurmurFrame::").skip(1).map(|rest| {
                rest.split(|c: char| !c.is_alphanumeric())
                    .next()
                    .unwrap_or("")
            });
            for name in named {
                let Some(frame) = StreamFrame::ALL
                    .into_iter()
                    .find(|f| format!("{f:?}") == name)
                else {
                    continue;
                };
                for &method in methods {
                    if !TransportKind::ALL
                        .into_iter()
                        .any(|transport| writes_frame(method, frame, transport))
                    {
                        uncarried.push(format!(
                            "{}:{}: {frame:?} on {method:?}: {}",
                            line.file, line.number, line.code
                        ));
                    }
                }
            }
        }
        assert!(
            uncarried.is_empty(),
            "frames written on a method writes_frame does not carry them on:\n{}",
            uncarried.join("\n")
        );
    }

    #[test]
    fn stream_frame_wire_names_are_distinct() {
        let names: std::collections::BTreeSet<&str> =
            StreamFrame::ALL.iter().map(|f| f.wire_name()).collect();
        assert_eq!(names.len(), StreamFrame::ALL.len());
    }
}
