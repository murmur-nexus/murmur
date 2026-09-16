use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use serde::Serialize;
use tokio::sync::broadcast;

pub(crate) type SseBroadcast = broadcast::Sender<Arc<String>>;

/// Cadence at which an idle SSE connection writes [`SSE_HEARTBEAT_COMMENT`].
///
/// Shared by `message/stream` and `stream/watch` so the two endpoints present one
/// liveness contract. Not configurable.
pub(crate) const SSE_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// The liveness signal written to an idle SSE connection.
///
/// An SSE comment, not a frame: it carries no `id:` line, is written straight to the
/// socket rather than through [`emit_sse`] or [`SseEventBuffer::push`], and so consumes
/// no event id and never enters the replay buffer. A client that counts events sees
/// none of these.
pub(crate) const SSE_HEARTBEAT_COMMENT: &[u8] = b":heartbeat\n\n";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskStatusUpdateEvent {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    pub status: StreamStatus,
    pub r#final: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StreamStatus {
    pub state: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskArtifactUpdateEvent {
    pub id: String,
    pub artifact: StreamArtifact,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StreamArtifact {
    pub tool_name: String,
    pub content: String,
    /// The fence source naming what produced `content` — `tool:<name>` — or `None` when
    /// `content` carries no fence. It tells apart the frame shapes that are otherwise
    /// identical on the wire: an ordinary tool result, a skill result, a dispatch failure and
    /// a hook artifact all arrive as a `tool_name` and a `content`.
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
    /// an error. A hook artifact reaches the model unfenced.
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

/// Format a single SSE event frame:
///   id: <event_id>\n
///   event: <event_type>\n
///   data: <json>\n\n
pub(crate) fn format_sse_event(event_id: u64, event_type: &str, data: &str) -> String {
    format!("id: {event_id}\nevent: {event_type}\ndata: {data}\n\n")
}

/// Result of a replay request from `SseEventBuffer::replay_from`.
pub(crate) enum ReplayResult {
    Complete(Vec<Arc<String>>),
    /// Some events before `first_available_id` were evicted from the buffer.
    /// Callers should emit a gap event before the replay events.
    WithGap {
        first_available_id: u64,
        events: Vec<Arc<String>>,
    },
}

/// Format a gap SSE event indicating that buffered history starts at `first_available_id`.
pub(crate) fn format_gap_event(first_available_id: u64) -> String {
    format!("event: gap\ndata: {{\"first_available_id\":{first_available_id}}}\n\n")
}

/// Bounded ring-buffer of pre-formatted SSE event strings for reconnect replay.
pub(crate) struct SseEventBuffer {
    events: VecDeque<(u64, Arc<String>)>,
    capacity: usize,
}

impl SseEventBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::new(),
            capacity,
        }
    }

    pub fn push(&mut self, id: u64, event: Arc<String>) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back((id, event));
    }

    /// Return all events with id > last_id, distinguishing a clean replay from one
    /// where earlier events have been evicted from the buffer.
    pub fn replay_from(&self, last_id: u64) -> ReplayResult {
        if self.events.is_empty() {
            return ReplayResult::Complete(vec![]);
        }

        let oldest_id = self.events.front().map(|(id, _)| *id).unwrap_or(0);

        if last_id < oldest_id.saturating_sub(1) {
            // Caller asked for events that have been evicted — signal a gap.
            let events = self.events.iter().map(|(_, s)| Arc::clone(s)).collect();
            return ReplayResult::WithGap {
                first_available_id: oldest_id,
                events,
            };
        }

        let events = self
            .events
            .iter()
            .filter(|(id, _)| *id > last_id)
            .map(|(_, s)| Arc::clone(s))
            .collect();
        ReplayResult::Complete(events)
    }
}

/// Serialize, buffer, and broadcast one SSE event. No-op when sse is None or no receivers.
pub(crate) async fn emit_sse(
    sse: &Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    event_id: &mut u64,
    event_type: &str,
    data: &impl Serialize,
) {
    let Some((sse_tx, sse_buffer)) = sse else {
        return;
    };
    let data_str = match serde_json::to_string(data) {
        Ok(s) => s,
        Err(_) => return,
    };
    let event_str = format_sse_event(*event_id, event_type, &data_str);
    let event_arc = Arc::new(event_str);
    {
        let mut buf = sse_buffer.lock().unwrap();
        buf.push(*event_id, Arc::clone(&event_arc));
    }
    let _ = sse_tx.send(event_arc); // ignore "no receivers" error
    *event_id += 1;
}

/// Check whether a pre-formatted SSE event string is a terminal status event.
/// Only `event: status` events with `"final":true` close the stream; text events
/// with `"final":true` (cursor-removal or non-streaming fallback) do not.
pub(crate) fn is_final_sse_event(event: &str) -> bool {
    event.contains("event: status\n") && event.contains("\"final\":true")
}

/// Emit one text chunk SSE event (synchronous, for use in `func_wrap` callbacks).
///
/// Wire format:
///   event: text
///   data: {"id":"<task_id>","text":"<chunk>","final":false}
pub(crate) fn emit_chunk_sse(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    event_id: &Arc<AtomicU64>,
    task_id: &str,
    chunk: &str,
) {
    let id = event_id.fetch_add(1, Ordering::Relaxed);
    let data = format_text_sse_data(task_id, chunk, false);
    let event_str = format_sse_event(id, "text", &data);
    let event_arc = Arc::new(event_str);
    buf.lock().unwrap().push(id, Arc::clone(&event_arc));
    let _ = tx.send(event_arc);
}

/// Emit one thinking chunk SSE event (synchronous).
///
/// Wire format:
///   event: thinking
///   data: {"id":"<task_id>","text":"<chunk>","final":false}
pub(crate) fn emit_thinking_chunk_sse(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    event_id: &Arc<AtomicU64>,
    task_id: &str,
    chunk: &str,
) {
    let id = event_id.fetch_add(1, Ordering::Relaxed);
    let data = format_text_sse_data(task_id, chunk, false);
    let event_str = format_sse_event(id, "thinking", &data);
    let event_arc = Arc::new(event_str);
    buf.lock().unwrap().push(id, Arc::clone(&event_arc));
    let _ = tx.send(event_arc);
}

/// Emit a final text SSE event (synchronous) — either cursor-removal (empty text) or
/// full-text fallback for non-streaming drivers.
///
/// Wire format:
///   event: text
///   data: {"id":"<task_id>","text":"<text>","final":true}
pub(crate) fn emit_chunk_sse_final(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    event_id: &Arc<AtomicU64>,
    task_id: &str,
    text: &str,
) {
    let id = event_id.fetch_add(1, Ordering::Relaxed);
    let data = format_text_sse_data(task_id, text, true);
    let event_str = format_sse_event(id, "text", &data);
    let event_arc = Arc::new(event_str);
    buf.lock().unwrap().push(id, Arc::clone(&event_arc));
    let _ = tx.send(event_arc);
}

fn format_text_sse_data(task_id: &str, text: &str, is_final: bool) -> String {
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    format!(r#"{{"id":"{task_id}","text":{text_json},"final":{is_final}}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_json(artifact: StreamArtifact) -> String {
        serde_json::to_string(&TaskArtifactUpdateEvent {
            id: "task_1".into(),
            artifact,
        })
        .unwrap()
    }

    #[test]
    fn tool_call_frame_serializes_every_key_in_declaration_order() {
        let json = frame_json(StreamArtifact::tool_call(
            "bash".into(),
            "hello".into(),
            Some("tool:bash".into()),
            "call_1",
            false,
            12,
            Some(0),
            false,
        ));
        assert_eq!(
            json,
            r#"{"id":"task_1","artifact":{"tool_name":"bash","content":"hello","fence_source":"tool:bash","tool_call_id":"call_1","is_error":false,"duration_ms":12,"exit_code":0,"truncated":false}}"#
        );
    }

    #[test]
    fn tool_call_frame_passes_truncated_through() {
        let json = frame_json(StreamArtifact::tool_call(
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
        let ran = frame_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "call_1",
            true,
            1204,
            Some(2),
            false,
        ));
        assert!(ran.contains(r#""exit_code":2"#), "{ran}");
        assert!(ran.contains(r#""is_error":true"#), "{ran}");

        let none = frame_json(StreamArtifact::tool_call(
            "write-file".into(),
            String::new(),
            None,
            "call_1",
            true,
            3,
            None,
            false,
        ));
        assert!(none.contains(r#""exit_code":null"#), "{none}");
    }

    #[test]
    fn tool_call_frame_records_an_empty_provider_id_as_null() {
        let empty = frame_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "",
            false,
            1,
            None,
            false,
        ));
        assert!(empty.contains(r#""tool_call_id":null"#), "{empty}");

        let issued = frame_json(StreamArtifact::tool_call(
            "bash".into(),
            String::new(),
            None,
            "toolu_01",
            false,
            1,
            None,
            false,
        ));
        assert!(issued.contains(r#""tool_call_id":"toolu_01""#), "{issued}");
    }

    #[test]
    fn hook_frame_reports_no_call_behind_it() {
        let json = frame_json(StreamArtifact::hook(
            "my-hook".into(),
            r#"{"reviewed":true}"#.into(),
        ));
        assert_eq!(
            json,
            r#"{"id":"task_1","artifact":{"tool_name":"my-hook","content":"{\"reviewed\":true}","fence_source":null,"tool_call_id":null,"is_error":false,"duration_ms":null,"exit_code":null,"truncated":false}}"#
        );
    }

    #[test]
    fn format_sse_event_produces_correct_frame() {
        let out = format_sse_event(3, "status", r#"{"id":"x"}"#);
        assert_eq!(out, "id: 3\nevent: status\ndata: {\"id\":\"x\"}\n\n");
    }

    #[test]
    fn event_buffer_evicts_oldest_when_full() {
        let mut buf = SseEventBuffer::new(2);
        buf.push(1, Arc::new("a".to_string()));
        buf.push(2, Arc::new("b".to_string()));
        buf.push(3, Arc::new("c".to_string())); // evicts id=1
                                                // Requesting from id=0 asks for events that were evicted → WithGap
        let result = buf.replay_from(0);
        let (first_id, events) = match result {
            ReplayResult::WithGap {
                first_available_id,
                events,
            } => (first_available_id, events),
            ReplayResult::Complete(_) => panic!("expected WithGap"),
        };
        assert_eq!(first_id, 2);
        assert_eq!(events.len(), 2);
        assert_eq!(*events[0], "b");
        assert_eq!(*events[1], "c");
    }

    #[test]
    fn replay_from_returns_events_after_last_id() {
        let mut buf = SseEventBuffer::new(128);
        buf.push(1, Arc::new("one".to_string()));
        buf.push(2, Arc::new("two".to_string()));
        buf.push(3, Arc::new("three".to_string()));
        // oldest_id=1, last_id=1: 1 < 1.saturating_sub(1)=0 is false → Complete
        let result = buf.replay_from(1);
        let events = match result {
            ReplayResult::Complete(events) => events,
            ReplayResult::WithGap { .. } => panic!("expected Complete"),
        };
        assert_eq!(events.len(), 2);
        assert_eq!(*events[0], "two");
        assert_eq!(*events[1], "three");
    }

    #[test]
    fn replay_from_no_gap_when_contiguous() {
        let mut buf = SseEventBuffer::new(3);
        buf.push(5, Arc::new("five".to_string()));
        buf.push(6, Arc::new("six".to_string()));
        // oldest_id=5, last_id=4: 4 < 5.saturating_sub(1)=4 is false → Complete (no gap)
        let result = buf.replay_from(4);
        let events = match result {
            ReplayResult::Complete(events) => events,
            ReplayResult::WithGap { .. } => panic!("expected Complete"),
        };
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn format_gap_event_has_correct_shape() {
        let s = format_gap_event(42);
        assert!(s.contains("event: gap\n"), "missing event field");
        assert!(s.contains("\"first_available_id\":42"), "missing id field");
        assert!(s.ends_with("\n\n"), "missing double newline terminator");
    }

    #[test]
    fn is_final_detects_status_final_only() {
        let status_final = "id: 5\nevent: status\ndata: {\"id\":\"x\",\"final\":true}\n\n";
        let text_final =
            "id: 3\nevent: text\ndata: {\"id\":\"x\",\"text\":\"\",\"final\":true}\n\n";
        let status_nonfinal = "id: 1\nevent: status\ndata: {\"id\":\"x\",\"final\":false}\n\n";

        assert!(
            is_final_sse_event(status_final),
            "status:final:true should be terminal"
        );
        assert!(
            !is_final_sse_event(text_final),
            "text:final:true should NOT be terminal"
        );
        assert!(
            !is_final_sse_event(status_nonfinal),
            "status:final:false should NOT be terminal"
        );
    }
}
