//! The A2A half of a `transport: process` attempt: the events its harness's events write.
//!
//! The same event constructors the http path uses, fed from the process driver contract's generic
//! events, so the same fact produces the same event on both transports. Nothing here names a
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
//! `thinking-delta`, `thinking`, `tool-call-started` or `tool-call` it sees — a fragment or a
//! started call opens one even though the runner does not count it as a turn, because the http
//! path writes its `working` status before the driver call and therefore before any chunk.
//! Segments are an A2A notion only: opening one reads the runner's turn counter and never advances
//! it.
//!
//! # A tool call being written
//!
//! `tool-call-started` and `tool-call-progress` tell the client a call is coming while the model
//! is still writing it. Neither carries any of the call's input: a hook or the call gate has not
//! seen the complete call yet, and may deny it. [`ToolCallProgress`] decides which reports become
//! frames, the same way it does for the http transport: progress is coalesced per call, and a start
//! for a call already started, written or answered, and progress for a call not being written,
//! write nothing.
//!
//! # The attempt's ending
//!
//! Every event this module writes leaves the task working. Every way an attempt can end records
//! one [`AttemptEnding`] — [`ending`] reads it off the attempt's own result on every path out of
//! the attempt — and the task's one terminal status is `run_task_with_reopens`'s, written from the
//! last attempt's ending once the `on-task-end` hooks have decided not to reopen the task.
//!
//! [`ending`]: A2aStream::ending
//!
//! # Text replaces, it never appends
//!
//! There is no replace primitive on this wire: a client concatenates `text` frames until one
//! arrives with `"final":true`. So a segment that streamed fragments closes with an empty
//! `final:true` frame — the cursor removal — and the client keeps what it was streamed, rather
//! than being sent the same words twice. The authoritative answer reaches it anyway, as the
//! task's `response` artifact and as `out/result.txt`. The same rule governs
//! `thinking`, whose frames are always `"final":false`.
//!
//! # Hook artifacts
//!
//! What the `on-inference` hooks returned for the turn `turn-end` closes reaches the client as one
//! `artifact` frame each, in hook-registration order, after the cursor removal that turn owed and
//! before the fallback text — the place the http path writes its last inference turn's. Only an
//! attempt that completes forwards them: a turn closed by a `tool-result` or by `turn-failed`, an
//! interrupted attempt and one stopped at a spend ceiling forward none.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use crate::{
    a2a::TaskState,
    agent::{
        failure_message, task_state_for, AgentLoopExit, AttemptEnding, COMPLETED_STATUS_MESSAGE,
    },
    cancel::CANCELED_STATUS_MESSAGE,
    errors::RuntimeError,
    hooks::HookArtifact,
    process_driver::{QuotaState, QuotaWindow},
    streaming::{
        emit_frame, emit_status, emit_text, emit_thinking, MurmurFrame, QuotaFrameWindow,
        StreamArtifact, StreamSink, StreamTask,
    },
    tool_call_progress::ToolCallProgress,
};

/// Where one attempt's frames go. Absent for a run with no A2A task — `mur run`, a `task.md`
/// launch — which writes nothing.
struct A2aTarget {
    sse: StreamSink,
    task: StreamTask,
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
    /// The calls the model is writing, and those whose complete `tool-call` or result has
    /// arrived.
    calls: ToolCallProgress,
}

impl A2aStream {
    /// The stream for one attempt. `task_id` and `context_id` are `None` for a run that serves
    /// no A2A task, which makes every method here a no-op.
    pub(super) fn new(
        sse: StreamSink,
        task_id: Option<String>,
        context_id: Option<String>,
        chunks_emitted: Arc<AtomicBool>,
    ) -> Self {
        Self {
            target: StreamTask::of(task_id.as_deref(), context_id.as_deref())
                .map(|task| A2aTarget { sse, task }),
            chunks_emitted,
            segment: Segment::default(),
            result: String::new(),
            interrupted: false,
            harness_killed: false,
            spend_refusal: None,
            calls: ToolCallProgress::default(),
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
        if let Some(target) = &self.target {
            emit_status(
                &target.sse,
                &target.task,
                TaskState::Working,
                &format!("inference turn {turn}"),
                None,
            );
        }
    }

    /// One streamed text fragment.
    pub(super) fn text_delta(&mut self, text: &str) {
        self.segment.streamed_text = true;
        self.segment.cursor_removal_owed = true;
        self.chunks_emitted.store(true, Ordering::Relaxed);
        if let Some(target) = &self.target {
            emit_text(&target.sse, &target.task, text, false);
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
        if let Some(target) = &self.target {
            emit_thinking(&target.sse, &target.task, text);
        }
    }

    /// A complete thinking. Sent only by a segment that streamed no thinking fragment; after one,
    /// it is a repeat of what the client has.
    pub(super) fn complete_thinking(&mut self, text: &str) {
        if self.segment.streamed_thinking {
            return;
        }
        if let Some(target) = &self.target {
            emit_thinking(&target.sse, &target.task, text);
        }
    }

    /// The model has begun writing tool call `id`, named `name`, in the segment numbered `turn`.
    ///
    /// Opens the segment, if none is open, and writes `tool-call-started`. A start with no id, or
    /// for a call already started, written or answered, writes nothing and opens nothing. Owes and
    /// pays no cursor removal: the text the segment streamed so far is not yet replaced.
    pub(super) async fn tool_call_started(&mut self, turn: u32, id: &str, name: &str) {
        if !self.calls.start(id, name) {
            return;
        }
        self.open_segment(turn).await;
        self.frame(MurmurFrame::ToolCallStarted {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
        });
    }

    /// The model has written `input_bytes` of started call `id`'s input, as of `now`. Writes a
    /// `tool-call-progress` frame when one is due; nothing for a call not being written.
    pub(super) async fn tool_call_progress(&mut self, id: &str, input_bytes: u64, now: Instant) {
        let Some(size) = self.calls.report(id, input_bytes, now) else {
            return;
        };
        self.progress(id, size);
    }

    /// Call `id`'s complete `tool-call` arrived at `now`. Writes the size still held for it, if
    /// any, and settles it. Returns the name its start carried, for a call that was started.
    pub(super) async fn tool_call_complete(&mut self, id: &str, now: Instant) -> Option<String> {
        let call = self.calls.complete(id, now)?;
        if let Some(size) = call.held {
            self.progress(id, size);
        }
        Some(call.name)
    }

    /// One finished tool call, which closes the segment that issued it.
    pub(super) async fn tool_result(&mut self, artifact: StreamArtifact) {
        if let Some(id) = artifact.tool_call_id.as_deref() {
            self.calls.settle(id);
        }
        self.pay_cursor_removal();
        self.segment.open = false;
        self.frame(MurmurFrame::Artifact(artifact));
    }

    /// The harness ended the turn. Holds the result for the attempt's ending, forwards what the
    /// `on-inference` hooks returned for the turn it closed, and sends the result as the whole of
    /// the answer when the last segment streamed the client nothing.
    pub(super) async fn turn_end(&mut self, result: &str, hook_artifacts: Vec<HookArtifact>) {
        self.pay_cursor_removal();
        self.segment.open = false;
        self.result = result.to_string();
        // An interrupted attempt delivers no answer: a person stopped it, and what the harness
        // produced anyway reaches them through `out/result.txt` rather than as final text.
        if self.interrupted {
            return;
        }
        for hook_artifact in hook_artifacts {
            // The operator's own declared hook speaking, so the frame carries no fence.
            self.frame(MurmurFrame::Artifact(StreamArtifact::hook(
                hook_artifact.hook_name,
                hook_artifact.payload,
            )));
        }
        if self.segment.streamed_text || result.is_empty() {
            return;
        }
        if let Some(target) = &self.target {
            emit_text(&target.sse, &target.task, result, true);
        }
    }

    /// One validated report of the subscription windows the harness says are used, sent as the
    /// driver reported it. Opens no segment and leaves the task working.
    pub(super) fn quota(&self, windows: &[QuotaWindow]) {
        self.frame(MurmurFrame::Quota {
            windows: windows
                .iter()
                .map(|window| QuotaFrameWindow {
                    name: window.name.clone(),
                    used: window.used,
                    resets_at_ms: window.resets_at,
                    window_minutes: window.window_minutes,
                    state: window.state.map(QuotaState::wire_name),
                })
                .collect(),
        });
    }

    /// The harness's prediction of what the person will ask next. Opens no segment. An
    /// interrupted attempt sends none, as it sends no answer: a person stopped the task.
    pub(super) fn suggested_prompt(&self, text: &str) {
        if self.interrupted {
            return;
        }
        self.frame(MurmurFrame::SuggestedPrompt {
            text: text.to_string(),
        });
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
                message: COMPLETED_STATUS_MESSAGE.to_string(),
                response: Some(self.result.clone()),
                no_answer: false,
            },
            TaskState::Canceled => AttemptEnding {
                state: TaskState::Canceled,
                message: canceled_message(self.harness_killed).to_string(),
                response: None,
                no_answer: false,
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
                no_answer: false,
            },
        }
    }

    /// The cursor removal the segment owes, if it still owes one.
    fn pay_cursor_removal(&mut self) {
        if !self.segment.cursor_removal_owed {
            return;
        }
        self.segment.cursor_removal_owed = false;
        if let Some(target) = &self.target {
            emit_text(&target.sse, &target.task, "", true);
        }
    }

    /// One murmur frame of the task, when the attempt serves one.
    fn frame(&self, frame: MurmurFrame) {
        if let Some(target) = &self.target {
            emit_frame(&target.sse, &target.task, frame);
        }
    }

    fn progress(&self, tool_call_id: &str, input_bytes: u64) {
        self.frame(MurmurFrame::ToolCallProgress {
            tool_call_id: tool_call_id.to_string(),
            input_bytes,
        });
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::sync::Mutex;

    use super::*;
    use crate::streaming::{buffered, event_kind, frame_of, SseEventBuffer};

    /// `ms` milliseconds after `base`.
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// A stream for one task, writing into a real buffer, and that buffer.
    fn stream() -> (A2aStream, Arc<Mutex<SseEventBuffer>>) {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let frames = Arc::new(Mutex::new(SseEventBuffer::new(64)));
        let stream = A2aStream::new(
            Some((tx, Arc::clone(&frames))),
            Some("tsk_test".to_string()),
            Some("ctx_test".to_string()),
            Arc::new(AtomicBool::new(false)),
        );
        (stream, frames)
    }

    /// The kind of every event the buffer holds, in order.
    fn event_types(frames: &Arc<Mutex<SseEventBuffer>>) -> Vec<String> {
        buffered(frames).iter().map(event_kind).collect()
    }

    #[tokio::test]
    async fn progress_a_report_after_the_complete_call_writes_nothing() {
        let (mut stream, frames) = stream();
        let now = Instant::now();
        stream.tool_call_started(1, "c1", "write_file").await;
        stream.tool_call_progress("c1", 12, now).await;
        assert_eq!(
            stream.tool_call_complete("c1", at(now, 1)).await.as_deref(),
            Some("write_file")
        );
        stream.tool_call_progress("c1", 40, at(now, 1_000)).await;
        assert_eq!(
            event_types(&frames),
            ["status-update", "tool-call-started", "tool-call-progress"]
        );
    }

    /// A quota report and a suggestion are one frame each, opening no segment, so no `working`
    /// status precedes them; the frame carries the report as sent, members left out stay out.
    #[tokio::test]
    async fn quota_and_suggestion_are_one_frame_each_and_open_no_segment() {
        let (stream, frames) = stream();
        stream.quota(&[
            QuotaWindow {
                name: "five-hour".into(),
                used: Some(0.5),
                resets_at: Some(1_792_497_600_000),
                window_minutes: None,
                state: Some(QuotaState::Exhausted),
            },
            QuotaWindow {
                name: "seven-day".into(),
                used: None,
                resets_at: None,
                window_minutes: None,
                state: None,
            },
        ]);
        stream.suggested_prompt("now run the tests");
        assert_eq!(event_types(&frames), ["quota", "suggested-prompt"]);
        let sent = buffered(&frames);
        assert_eq!(
            frame_of(&sent[0]),
            serde_json::json!({"frame": "quota", "windows": [
                {"name": "five-hour", "used": 0.5, "resetsAtMs": 1_792_497_600_000_u64,
                 "state": "exhausted"},
                {"name": "seven-day"}
            ]})
        );
        assert_eq!(
            sent[0]["statusUpdate"]["status"]["state"],
            "TASK_STATE_WORKING"
        );
        assert_eq!(
            frame_of(&sent[1]),
            serde_json::json!({"frame": "suggested-prompt", "text": "now run the tests"})
        );
    }

    /// An interrupted attempt forwards no suggestion, but still forwards a quota report.
    #[tokio::test]
    async fn an_interrupted_attempt_forwards_quota_and_no_suggestion() {
        let (mut stream, frames) = stream();
        stream.mark_interrupted();
        stream.suggested_prompt("ask me more");
        stream.quota(&[QuotaWindow {
            name: "five-hour".into(),
            used: Some(1.0),
            resets_at: None,
            window_minutes: None,
            state: None,
        }]);
        assert_eq!(event_types(&frames), ["quota"]);
    }

    /// A start the stream does not believe opens no segment, so it writes no `working` status.
    #[tokio::test]
    async fn a_start_after_the_complete_call_opens_no_segment() {
        let (mut stream, frames) = stream();
        assert_eq!(stream.tool_call_complete("c1", Instant::now()).await, None);
        stream.tool_call_started(1, "c1", "write_file").await;
        assert!(event_types(&frames).is_empty());
    }
}
