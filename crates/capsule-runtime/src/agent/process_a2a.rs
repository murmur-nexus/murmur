//! The A2A half of a `transport: process` attempt: the frames its harness's events write.
//!
//! The same frame builders the http path uses, fed from the process driver contract's generic
//! events, so the same fact produces the same frame on both transports. Nothing here names a
//! harness, a driver or a vendor: every value on the wire comes from an [`Event`] field or from
//! the runtime's own measurement.
//!
//! [`Event`]: crate::process_driver::Event
//!
//! # Segments
//!
//! One piece of state is kept per **segment**: the run of events between the start of the run (or
//! the last `tool-result`) and the next `tool-result` or terminal event. A segment is this
//! transport's equivalent of one http inference turn, and opens at the first `text-delta`, `text`,
//! `thinking-delta`, `thinking` or `tool-call` it sees — a fragment opens one even though the
//! runner does not count it as a turn, because the http path writes its `working` status before
//! the driver call and therefore before any chunk. Segments are an A2A notion only: opening one
//! reads the runner's turn counter and never advances it.
//!
//! # The one terminal status
//!
//! Every way an attempt can end writes exactly one `final:true` `status` frame, and [`finish`] is
//! the only place that writes one. It is called on every path out of the attempt with the
//! attempt's own result, so a client waiting on a terminal status cannot be left waiting.
//!
//! [`finish`]: A2aStream::finish
//!
//! # Text replaces, it never appends
//!
//! There is no replace primitive on this wire: a client concatenates `text` frames until one
//! arrives with `"final":true`. So a segment that streamed fragments closes with an empty
//! `final:true` frame — the cursor removal — and the client keeps what it was streamed, rather
//! than being sent the same words twice. The authoritative answer reaches it anyway, as the
//! `response` of the `completed` status and as `out/result.txt`. The same rule governs
//! `thinking`, whose frames are always `"final":false`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use crate::{
    agent::AgentLoopExit,
    errors::RuntimeError,
    streaming::{
        emit_chunk_sse, emit_chunk_sse_final, emit_sse, emit_thinking_chunk_sse, SseBroadcast,
        SseEventBuffer, StreamArtifact, StreamStatus, TaskArtifactUpdateEvent,
        TaskStatusUpdateEvent,
    },
};

/// The diagnostic codes a reader of a terminal `failed` message needs to look the failure up.
/// The authority for which code renders which error is murmur-cli's `CliError` mapping; these
/// three are the errors this transport raises once an attempt is under way.
const E_RUN_033: &str = "E-RUN-033";
const E_RUN_034: &str = "E-RUN-034";
const E_RUN_035: &str = "E-RUN-035";

/// Where one attempt's frames go. Absent for a run with no A2A task — `mur run`, a `task.md`
/// launch — which writes nothing.
struct A2aTarget {
    sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    task_id: String,
    context_id: Option<String>,
}

/// What the current segment has already sent, which is what decides the frames the rest of it
/// produces. Reset wholesale when a segment opens.
#[derive(Default)]
struct Segment {
    /// Whether a segment is open. The flags below outlive the segment that set them: `turn-end`
    /// reads the last segment's, open or closed.
    open: bool,
    /// Whether this segment streamed a `text-delta`. A segment that streamed one has already sent
    /// the client its text, so `turn-end` sends no fallback text frame.
    streamed_text: bool,
    /// Whether the cursor removal this segment owes is still unsent. Owed by the first
    /// `text-delta`, paid exactly once, by whichever of `text`, `tool-result` or the terminal
    /// event comes first.
    cursor_removal_owed: bool,
    /// Whether this segment streamed a `thinking-delta`, which makes a complete `thinking` a
    /// repeat of what the client already has.
    streamed_thinking: bool,
}

/// The broadcast, buffer and task id the synchronous chunk emitters take.
struct Wire<'a> {
    tx: &'a SseBroadcast,
    buf: &'a Arc<Mutex<SseEventBuffer>>,
    task_id: &'a str,
}

/// One attempt's A2A stream.
pub(super) struct A2aStream {
    target: Option<A2aTarget>,
    /// The http path's per-turn streaming flag, kept in step on this transport too: cleared when
    /// a segment opens, set by every fragment streamed inside one.
    chunks_emitted: Arc<AtomicBool>,
    segment: Segment,
    /// The result the terminal `completed` status carries, as the harness reported it.
    result: String,
}

impl A2aStream {
    /// The stream for one attempt. `task_id` is `None` for a run that serves no A2A task, which
    /// makes every method here a no-op.
    pub(super) fn new(
        sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
        task_id: Option<String>,
        context_id: Option<String>,
        chunks_emitted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            target: task_id.map(|task_id| A2aTarget {
                sse,
                task_id,
                context_id,
            }),
            chunks_emitted,
            segment: Segment::default(),
            result: String::new(),
        }
    }

    /// Open a segment, if none is open, for the turn number a turn opening now would take.
    ///
    /// Writes the `working` status the http path writes before a driver dispatch, and clears the
    /// streaming flag that dispatch clears.
    pub(super) async fn open_segment(&mut self, turn: u32) {
        if self.segment.open {
            return;
        }
        self.segment = Segment {
            open: true,
            ..Segment::default()
        };
        self.chunks_emitted.store(false, Ordering::Relaxed);
        self.status("working", &format!("inference turn {turn}"), None, false)
            .await;
    }

    /// One streamed text fragment.
    pub(super) fn text_delta(&mut self, text: &str) {
        self.segment.streamed_text = true;
        self.segment.cursor_removal_owed = true;
        self.chunks_emitted.store(true, Ordering::Relaxed);
        if let Some(wire) = self.wire() {
            emit_chunk_sse(wire.tx, wire.buf, wire.task_id, text);
        }
    }

    /// A complete text. It replaces what the segment streamed rather than adding to it, so the
    /// client is sent the cursor removal and keeps the words it already has.
    pub(super) fn complete_text(&mut self) {
        self.pay_cursor_removal();
    }

    /// One streamed thinking fragment.
    pub(super) fn thinking_delta(&mut self, text: &str) {
        self.segment.streamed_thinking = true;
        if let Some(wire) = self.wire() {
            emit_thinking_chunk_sse(wire.tx, wire.buf, wire.task_id, text);
        }
    }

    /// A complete thinking. Sent only by a segment that streamed no thinking fragment; after one,
    /// it is a repeat of what the client has.
    pub(super) fn complete_thinking(&mut self, text: &str) {
        if self.segment.streamed_thinking {
            return;
        }
        if let Some(wire) = self.wire() {
            emit_thinking_chunk_sse(wire.tx, wire.buf, wire.task_id, text);
        }
    }

    /// One finished tool call, which closes the segment that issued it.
    pub(super) async fn tool_result(&mut self, artifact: StreamArtifact) {
        self.pay_cursor_removal();
        self.segment.open = false;
        let Some(target) = self.target.as_ref() else {
            return;
        };
        emit_sse(
            &target.sse,
            "artifact",
            &TaskArtifactUpdateEvent {
                id: target.task_id.clone(),
                artifact,
            },
        )
        .await;
    }

    /// The harness ended the turn. Holds the result for the terminal status, and sends it as the
    /// whole of the answer when the last segment streamed the client nothing.
    pub(super) fn turn_end(&mut self, result: &str) {
        self.pay_cursor_removal();
        self.segment.open = false;
        self.result = result.to_string();
        if self.segment.streamed_text || result.is_empty() {
            return;
        }
        if let Some(wire) = self.wire() {
            emit_chunk_sse_final(wire.tx, wire.buf, wire.task_id, result);
        }
    }

    /// The harness failed the turn. The terminal status itself is [`Self::finish`]'s, which the
    /// attempt reaches by returning the failure as an error.
    pub(super) fn turn_failed(&mut self) {
        self.pay_cursor_removal();
        self.segment.open = false;
    }

    /// Write the attempt's one terminal status. Called on every path out of the attempt, with
    /// what the attempt returned, and never twice: this is the frame a client waits on.
    pub(super) async fn finish(&mut self, outcome: &Result<AgentLoopExit, RuntimeError>) {
        match outcome {
            Ok(_) => {
                let response = self.result.clone();
                self.status("completed", "session ended", Some(response), true)
                    .await;
            }
            Err(error) => {
                self.status("failed", &failure_message(error), None, true)
                    .await;
            }
        }
    }

    /// The cursor removal the segment owes, if it still owes one.
    fn pay_cursor_removal(&mut self) {
        if !self.segment.cursor_removal_owed {
            return;
        }
        self.segment.cursor_removal_owed = false;
        if let Some(wire) = self.wire() {
            emit_chunk_sse_final(wire.tx, wire.buf, wire.task_id, "");
        }
    }

    async fn status(&self, state: &str, message: &str, response: Option<String>, last: bool) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        emit_sse(
            &target.sse,
            "status",
            &TaskStatusUpdateEvent {
                id: target.task_id.clone(),
                context_id: target.context_id.clone(),
                status: StreamStatus {
                    state: state.to_string(),
                    message: message.to_string(),
                    response,
                },
                r#final: last,
            },
        )
        .await;
    }

    /// What the chunk emitters take, for a task whose stream has somewhere to go.
    fn wire(&self) -> Option<Wire<'_>> {
        let target = self.target.as_ref()?;
        let (tx, buf) = target.sse.as_ref()?;
        Some(Wire {
            tx,
            buf,
            task_id: &target.task_id,
        })
    }
}

/// What a failed attempt's terminal status says: the diagnostic the run failed with, under the
/// code a reader looks it up by.
fn failure_message(error: &RuntimeError) -> String {
    match diagnostic_code(error) {
        Some(code) => format!("error[{code}]: {error}"),
        None => error.to_string(),
    }
}

/// The code murmur-cli renders `error` under, for the failures this transport raises once an
/// attempt is under way. Every other error reaches the client as its message alone.
fn diagnostic_code(error: &RuntimeError) -> Option<&'static str> {
    match error {
        RuntimeError::HarnessTurnFailed { .. } => Some(E_RUN_033),
        RuntimeError::ProcessDriverCallFailed { .. } => Some(E_RUN_034),
        RuntimeError::ProcessHarnessInactive { .. } => Some(E_RUN_035),
        _ => None,
    }
}
