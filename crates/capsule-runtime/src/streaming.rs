use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
/// socket rather than through [`emit_frame`], and so consumes
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

/// Format an SSE event frame with no `id:` line, for a frame that is written straight to one
/// connection and never takes a place in the session's sequence.
pub(crate) fn format_unnumbered_sse_event(event_type: &str, data: &str) -> String {
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// The id of a frame formatted by [`format_sse_event`], read from its leading `id:` line, or
/// `None` for a frame that carries no id.
pub(crate) fn frame_id(frame: &str) -> Option<u64> {
    frame.strip_prefix("id: ")?.split('\n').next()?.parse().ok()
}

/// Decide whether a live frame is written to a connection that has already written every id up
/// to `last_written`, and advance `last_written` when it is.
///
/// A frame whose id is at or below `last_written` was already written by the replay (a frame
/// emitted between subscribing and reading the buffer arrives both ways), so it is dropped. A
/// frame with no id is always written and leaves `last_written` unchanged.
pub(crate) fn admit_live_frame(last_written: &mut Option<u64>, frame: &str) -> bool {
    let Some(id) = frame_id(frame) else {
        return true;
    };
    if last_written.is_some_and(|written| id <= written) {
        return false;
    }
    *last_written = Some(id);
    true
}

/// Result of a replay request from `SseEventBuffer::replay_from`.
pub(crate) enum ReplayResult {
    Complete(Vec<Arc<String>>),
    /// Frames after the cursor are no longer buffered, or the cursor was never issued by this
    /// session. `events` is the whole buffer; callers write a gap event before it.
    WithGap {
        first_available_id: u64,
        events: Vec<Arc<String>>,
    },
}

/// Format a gap SSE event indicating that buffered history starts at `first_available_id`.
pub(crate) fn format_gap_event(first_available_id: u64) -> String {
    format!("event: gap\ndata: {{\"first_available_id\":{first_available_id}}}\n\n")
}

/// Format the frame telling one SSE connection it lost `missed` live frames.
///
/// Written straight to that connection's socket, immediately before the next live frame
/// it receives: it carries no `id:` line, never passes through [`emit_frame`], and so
/// consumes no event id, never appears in a replay and never reaches any other connection.
/// `missed` is the count carried by the broadcast receiver's `RecvError::Lagged`, always at
/// least 1.
pub(crate) fn format_lagged_event(missed: u64) -> String {
    format!("event: lagged\ndata: {{\"missed\":{missed}}}\n\n")
}

/// The session's event-id sequence and a bounded ring-buffer of the most recent frames for
/// reconnect replay.
///
/// One buffer serves a whole capsule session, so every numbered frame of every task takes its
/// id from `next_id`: the first frame is `1` and each later one is exactly one higher.
pub(crate) struct SseEventBuffer {
    events: VecDeque<(u64, Arc<String>)>,
    capacity: usize,
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

    /// Number, format and store one frame, returning its id and the formatted text.
    fn append(&mut self, event_type: &str, data: &str) -> (u64, Arc<String>) {
        let id = self.next_id;
        self.next_id += 1;
        let event = Arc::new(format_sse_event(id, event_type, data));
        if self.events.len() >= self.capacity {
            self.events.pop_front();
        }
        self.events.push_back((id, Arc::clone(&event)));
        (id, event)
    }

    /// Return the frames written after `last_id`, distinguishing a clean replay from one that
    /// cannot be: frames after `last_id` were evicted, or `last_id` was never issued by this
    /// session. Either of those returns the whole buffer as [`ReplayResult::WithGap`].
    pub fn replay_from(&self, last_id: u64) -> ReplayResult {
        let Some(oldest_id) = self.events.front().map(|(id, _)| *id) else {
            return ReplayResult::Complete(vec![]);
        };

        if last_id >= self.next_id || last_id < oldest_id - 1 {
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

/// Give one frame the session's next id, buffer it and broadcast it, returning the id.
///
/// The buffer lock is held across the broadcast send, so frames emitted concurrently — by the
/// agent loop, a tool's chunk host function and a `request-input` wait — enter the buffer and
/// reach every live receiver in id order. A send with no receivers is not an error.
pub(crate) fn emit_frame(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    event_type: &str,
    data: &str,
) -> u64 {
    let mut buf = buf.lock().unwrap();
    let (id, event) = buf.append(event_type, data);
    let _ = tx.send(event);
    id
}

/// Serialize and emit one SSE event through [`emit_frame`]. No-op when sse is None.
pub(crate) async fn emit_sse(
    sse: &Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
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
    emit_frame(sse_tx, sse_buffer, event_type, &data_str);
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
    task_id: &str,
    chunk: &str,
) {
    emit_frame(
        tx,
        buf,
        "text",
        &format_text_sse_data(task_id, chunk, false),
    );
}

/// Emit one thinking chunk SSE event (synchronous).
///
/// Wire format:
///   event: thinking
///   data: {"id":"<task_id>","text":"<chunk>","final":false}
pub(crate) fn emit_thinking_chunk_sse(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    task_id: &str,
    chunk: &str,
) {
    emit_frame(
        tx,
        buf,
        "thinking",
        &format_text_sse_data(task_id, chunk, false),
    );
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
    task_id: &str,
    text: &str,
) {
    emit_frame(tx, buf, "text", &format_text_sse_data(task_id, text, true));
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

    /// A buffer of `capacity` holding `count` frames, ids `1..=count`, each frame's data its id.
    fn buffer_with(capacity: usize, count: u64) -> SseEventBuffer {
        let mut buf = SseEventBuffer::new(capacity);
        for n in 1..=count {
            buf.append("status", &n.to_string());
        }
        buf
    }

    fn ids(events: &[Arc<String>]) -> Vec<u64> {
        events.iter().map(|e| frame_id(e).unwrap()).collect()
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
        let (tx, mut rx) = broadcast::channel(8);
        let buf = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        assert_eq!(emit_frame(&tx, &buf, "status", "{}"), 1);
        emit_chunk_sse(&tx, &buf, "t", "a");
        emit_thinking_chunk_sse(&tx, &buf, "t", "b");
        emit_chunk_sse_final(&tx, &buf, "t", "");
        assert_eq!(emit_frame(&tx, &buf, "artifact", "{}"), 5);
        let received: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| frame_id(&e).unwrap())
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
    fn replay_from_zero_without_eviction_is_every_frame() {
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
        let (tx, mut rx) = broadcast::channel(total as usize);
        let buf = Arc::new(Mutex::new(SseEventBuffer::new(total as usize)));
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let (tx, buf) = (tx.clone(), Arc::clone(&buf));
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        if t % 2 == 0 {
                            emit_chunk_sse(&tx, &buf, "t", "x");
                        } else {
                            emit_frame(&tx, &buf, "status", "{}");
                        }
                    }
                });
            }
        });

        let buffered = complete(buf.lock().unwrap().replay_from(0));
        let received: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| frame_id(&e).unwrap())
            .collect();
        let expected: Vec<u64> = (1..=total).collect();
        assert_eq!(buffered, expected, "buffer order is id order");
        assert_eq!(received, expected, "live delivery order is id order");
    }

    #[test]
    fn frame_id_reads_the_leading_id_line() {
        assert_eq!(frame_id("id: 42\nevent: status\ndata: {}\n\n"), Some(42));
        assert_eq!(frame_id("event: gap\ndata: {}\n\n"), None);
        assert_eq!(frame_id(":heartbeat\n\n"), None);
    }

    #[test]
    fn live_frames_already_replayed_are_dropped() {
        let frame = |id| format_sse_event(id, "status", "{}");
        let mut last = Some(3);
        assert!(!admit_live_frame(&mut last, &frame(2)));
        assert!(!admit_live_frame(&mut last, &frame(3)));
        assert!(admit_live_frame(&mut last, &frame(4)));
        assert_eq!(last, Some(4));
        assert!(
            !admit_live_frame(&mut last, &frame(4)),
            "an id is written once"
        );
        assert!(admit_live_frame(&mut last, &format_gap_event(1)));
        assert_eq!(last, Some(4), "a frame without an id leaves the cursor");

        let mut fresh = None;
        assert!(admit_live_frame(&mut fresh, &frame(1)));
        assert_eq!(fresh, Some(1));
    }

    #[test]
    fn unnumbered_frame_has_no_id_line() {
        let out = format_unnumbered_sse_event("status", r#"{"id":"x"}"#);
        assert_eq!(out, "event: status\ndata: {\"id\":\"x\"}\n\n");
        assert_eq!(frame_id(&out), None);
    }

    #[test]
    fn format_gap_event_has_correct_shape() {
        let s = format_gap_event(42);
        assert!(s.contains("event: gap\n"), "missing event field");
        assert!(s.contains("\"first_available_id\":42"), "missing id field");
        assert!(s.ends_with("\n\n"), "missing double newline terminator");
    }

    #[test]
    fn format_lagged_event_has_exact_wire_form() {
        let s = format_lagged_event(7);
        assert_eq!(s, "event: lagged\ndata: {\"missed\":7}\n\n");
        let data = s
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data line");
        let parsed: serde_json::Value = serde_json::from_str(data).expect("data is JSON");
        assert_eq!(parsed, serde_json::json!({"missed": 7}));
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
