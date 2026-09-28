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
//! # The attempt's ending
//!
//! Every frame this module writes is non-final. Every way an attempt can end records one
//! [`AttemptEnding`] — [`ending`] reads it off the attempt's own result on every path out of the
//! attempt — and the task's one `final:true` `status` frame is `run_task_with_reopens`'s, written
//! from the last attempt's ending once the `on-task-end` hooks have decided not to reopen the
//! task.
//!
//! [`ending`]: A2aStream::ending
//!
//! # Text replaces, it never appends
//!
//! There is no replace primitive on this wire: a client concatenates `text` frames until one
//! arrives with `"final":true`. So a segment that streamed fragments closes with an empty
//! `final:true` frame — the cursor removal — and the client keeps what it was streamed, rather
//! than being sent the same words twice. The authoritative answer reaches it anyway, as the
//! `response` of the task's final `completed` status and as `out/result.txt`. The same rule governs
//! `thinking`, whose frames are always `"final":false`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use crate::{
    a2a::TaskState,
    agent::{
        failure_message, task_state_for, AgentLoopExit, AttemptEnding, SESSION_ENDED_STATUS_MESSAGE,
    },
    cancel::CANCELED_STATUS_MESSAGE,
    errors::RuntimeError,
    streaming::{
        emit_chunk_sse, emit_chunk_sse_final, emit_sse, emit_thinking_chunk_sse, SseBroadcast,
        SseEventBuffer, StreamArtifact, StreamStatus, TaskArtifactUpdateEvent,
        TaskStatusUpdateEvent,
    },
};

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
    /// The result a `completed` ending carries, as the harness reported it.
    result: String,
    /// Whether the runtime interrupted this attempt. Set before the interrupt goes out, so
    /// everything after it is a stopped task's, not an answer's: the harness's own last words are
    /// kept in `out/result.txt` and the trace, and are not sent as the client's final text.
    interrupted: bool,
    /// Whether the harness had to be killed rather than ending when it was asked, which is the
    /// one thing a `canceled` ending says beyond the fact of the cancel.
    harness_killed: bool,
    /// The refusal a reached spend ceiling stopped this attempt with, as its ending reports it.
    /// A2A has no state for a task stopped by policy, so it is `failed` carrying the refusal —
    /// the same ending the http path records for the same fact.
    spend_refusal: Option<String>,
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
            interrupted: false,
            harness_killed: false,
            spend_refusal: None,
        }
    }

    /// The runtime is about to interrupt the harness: this attempt ends `canceled`, whatever the
    /// harness says next.
    pub(super) fn mark_interrupted(&mut self) {
        self.interrupted = true;
    }

    /// The interrupted harness had to be killed. Adds the clause the client needs to know its
    /// session may not resume cleanly.
    pub(super) fn mark_harness_killed(&mut self) {
        self.harness_killed = true;
    }

    /// A spend ceiling stopped this attempt, with `refusal` as the reason its ending carries.
    pub(super) fn mark_spend_refused(&mut self, refusal: &crate::spend::SpendRefusal) {
        self.spend_refusal = Some(refusal.to_string());
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
        self.status("working", &format!("inference turn {turn}"))
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

    /// The harness ended the turn. Holds the result for the attempt's ending, and sends it as the
    /// whole of the answer when the last segment streamed the client nothing.
    pub(super) fn turn_end(&mut self, result: &str) {
        self.pay_cursor_removal();
        self.segment.open = false;
        self.result = result.to_string();
        // An interrupted attempt delivers no answer: a person stopped it, and what the harness
        // produced anyway reaches them through `out/result.txt` rather than as final text.
        if self.interrupted || self.segment.streamed_text || result.is_empty() {
            return;
        }
        if let Some(wire) = self.wire() {
            emit_chunk_sse_final(wire.tx, wire.buf, wire.task_id, result);
        }
    }

    /// The harness failed the turn. The attempt's ending is [`Self::ending`]'s, which the attempt
    /// reaches by returning the failure as an error.
    pub(super) fn turn_failed(&mut self) {
        self.pay_cursor_removal();
        self.segment.open = false;
    }

    /// How the attempt that returned `outcome` ended. Read once, on every path out of the
    /// attempt, whether or not it serves an A2A task.
    pub(super) fn ending(&self, outcome: &Result<AgentLoopExit, RuntimeError>) -> AttemptEnding {
        match task_state_for(outcome) {
            TaskState::Completed => AttemptEnding {
                state: TaskState::Completed,
                message: SESSION_ENDED_STATUS_MESSAGE.to_string(),
                response: Some(self.result.clone()),
            },
            TaskState::Canceled => AttemptEnding {
                state: TaskState::Canceled,
                message: canceled_message(self.harness_killed).to_string(),
                response: None,
            },
            _ => AttemptEnding {
                state: TaskState::Failed,
                message: match outcome {
                    Ok(AgentLoopExit::SpendCeilingReached) => {
                        self.spend_refusal.clone().unwrap_or_default()
                    }
                    Ok(exit) => exit.as_str().to_string(),
                    Err(error) => failure_message(error),
                },
                response: None,
            },
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

    async fn status(&self, state: &str, message: &str) {
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
                    response: None,
                    reopen: None,
                },
                r#final: false,
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

/// What a `canceled` ending says.
///
/// A harness that stopped when it was asked leaves the frame byte-identical to the one the http
/// path writes, so a client cannot tell the transports apart. One that had to be killed says so:
/// its own session was cut off mid-turn and the harness may not be able to resume it.
fn canceled_message(harness_killed: bool) -> &'static str {
    if harness_killed {
        CANCELED_KILLED_STATUS_MESSAGE
    } else {
        CANCELED_STATUS_MESSAGE
    }
}

/// [`canceled_message`] for a harness the runtime had to kill.
const CANCELED_KILLED_STATUS_MESSAGE: &str =
    "task canceled; the harness was killed and its session may not resume cleanly";
