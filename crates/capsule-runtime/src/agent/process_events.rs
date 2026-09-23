//! What the runtime does with the typed events a process driver reads out of its harness.
//!
//! The driver turns harness output into [`Event`]s; this module is the other half — it turns
//! those events into everything a turn records: the `inference` and `tool_call` trace events, the
//! `Inference` and `ToolCall` hooks, the OTel spans, the result text, the harness's own
//! diagnostics, and, through [`A2aStream`], the task's A2A frames. The runtime never reads the
//! harness's output itself, so nothing here interprets text; it only counts turns and writes
//! records.
//!
//! # Turns
//!
//! One logical turn is one model action, counted as the `transport: http` path counts it. A turn
//! opens at the first `text` or `tool-call` since the run started or since the last `tool-result`,
//! and closes at the next `tool-result` or terminal event. Thinking and the streaming deltas never
//! open one: a harness that streams its reasoning as its own events would otherwise burn a turn
//! per thought, and `max_turns` would mean something different on each transport.
//!
//! An A2A **segment** is a separate notion, held by [`A2aStream`]: it reads the turn counter to
//! number itself and never advances it, and a streamed fragment opens one where it opens no turn.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use murmur_artifact::{security_warning_link, W_SEC_031};
use serde_json::Value;

use crate::{
    agent::DriverUsage,
    errors::RuntimeError,
    hooks::{HookEvent, HookRuntime},
    otel::OtelEmitter,
    process_driver::{Event, FailureKind, Usage},
    spend::{SpendMeter, SpendRefusal},
    streaming::StreamArtifact,
    trace::TraceWriter,
};

use super::process::RunSession;
use super::process_a2a::A2aStream;

/// The billing mode the process transport exists for. Compared as a plain string: what any other
/// value means is the harness's business, and the runtime only reports that it is not this one.
const SUBSCRIPTION_AUTH: &str = "subscription";

/// What a batch of events did to the run.
pub(super) enum SinkOutcome {
    /// Nothing terminal; keep reading.
    Continue,
    /// The harness finished the turn. Carries the result text, already recorded.
    Ended,
    /// The harness reported the turn as failed.
    Failed(RuntimeError),
    /// A turn opened past the attempt's remaining budget. The harness is killed at once rather
    /// than given the exit grace: it is mid-turn and would otherwise keep spending.
    TurnBudgetExceeded(RuntimeError),
    /// A turn closed on or past a spend ceiling. Killed at once for the same reason a turn
    /// budget is: every further turn is spend the operator said must not happen.
    SpendCeilingReached(SpendRefusal),
}

/// One set of token counts, each member independently optional.
///
/// Used twice by [`ProcessEventSink`]: as the latest cumulative report the driver made for the
/// harness run, and as the totals already attributed to turns that have closed. `None` means the
/// harness does not report that count at all — never that it reported zero, which is `Some(0)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct UsageCounts {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    thinking: Option<u64>,
}

impl UsageCounts {
    /// The members in a fixed order, so growth and replacement walk the same list.
    fn members(&self) -> [Option<u64>; 5] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_creation,
            self.thinking,
        ]
    }

    fn from_members(members: [Option<u64>; 5]) -> Self {
        let [input, output, cache_read, cache_creation, thinking] = members;
        Self {
            input,
            output,
            cache_read,
            cache_creation,
            thinking,
        }
    }

    /// Replace each member `report` names, leaving the rest standing.
    ///
    /// A harness that reports output on one line and a full set on the next must not lose the
    /// earlier members, and a member missing from a later report is the harness declining to
    /// repeat itself rather than retracting what it said.
    fn overlay(&mut self, report: &Usage) {
        let incoming = [
            report.input,
            report.output,
            report.cache_read,
            report.cache_creation,
            report.thinking,
        ];
        let held = self.members();
        *self = Self::from_members(std::array::from_fn(|i| incoming[i].or(held[i])));
    }

    /// What has been reported since `attributed` was last taken, member by member, advancing
    /// `attributed` by exactly that.
    ///
    /// Every reported member is the total for the harness run so far, so only its growth belongs
    /// to the turn closing now: reporting the same total twice adds nothing, and a member that
    /// shrank contributes zero and leaves the larger total attributed, so a later report back at
    /// the earlier figure cannot be counted a second time.
    fn take_growth(&self, attributed: &mut Self) -> Self {
        let latest = self.members();
        let mut counted = attributed.members();
        let growth = std::array::from_fn(|i| {
            let latest = latest[i]?;
            let already = counted[i].unwrap_or(0);
            let grown = latest.saturating_sub(already);
            counted[i] = Some(already.saturating_add(grown));
            Some(grown)
        });
        *attributed = Self::from_members(counted);
        Self::from_members(growth)
    }
}

/// A tool call the harness reported, waiting for its result so the pair can be recorded with a
/// duration the runtime measured itself.
struct PendingToolCall {
    name: String,
    input: Value,
    input_bytes: u64,
    turn: u32,
    started: Instant,
}

/// The turn currently open: which index it will be recorded under, the text it produced, and
/// whether it called anything.
struct OpenTurn {
    index: u32,
    /// The last complete `text` event of the turn. A `text` replaces what came before it.
    text: Option<String>,
    /// The first tool this turn called, which names the turn's `tool_call` decision.
    first_tool: Option<String>,
}

/// Turns the driver's events into the records of one harness run.
///
/// Holds no harness knowledge: names arrive bare and are recorded as given, and the only text it
/// ever reads is the result it writes out.
pub(super) struct ProcessEventSink<'a> {
    /// The internal session workdir, where `out/result.txt` goes.
    workdir: PathBuf,
    /// This attempt's remaining turn budget, already net of the turns earlier attempts spent.
    max_turns: u32,
    /// How many turns have opened in this run.
    turns: u32,
    open: Option<OpenTurn>,
    pending: HashMap<String, PendingToolCall>,
    /// Set by the first terminal event, so the rest of its batch is ignored.
    finished: bool,
    /// The conversation this run belongs to, and where its session id is remembered.
    session: RunSession,
    /// The id the harness reported for this run, if it reported one at all. What it says wins
    /// over what the runtime asked for, and its absence is half of the session-gone predicate.
    /// Held rather than stored on arrival: a harness names its session before the model has
    /// produced anything, and a turn interrupted in that window leaves the harness holding no
    /// conversation to answer to.
    reported_session: Option<String>,
    /// The latest cumulative counts the driver has reported for this run, member by member.
    usage: UsageCounts,
    /// The part of [`Self::usage`] already attributed to turns that have closed. The difference
    /// between the two is what the next turn to close records.
    attributed: UsageCounts,
    /// This session's spend account, charged with each closed turn's growth and then asked
    /// whether a ceiling has been reached.
    spend: Arc<SpendMeter>,
    /// Whether this run produced observable work — see [`produces_observable_work`]. The one
    /// condition on committing the session: a run that produced nothing established no
    /// conversation, whatever it was told its session was called.
    produced: bool,
    /// Whether the run ended because the harness could not find the session it was handed. That
    /// entry is left exactly as it was, so nothing is written over it here.
    session_gone: bool,
    /// The A2A half of the same events. Borrowed rather than owned, because the attempt writes
    /// its one terminal status through it after this sink is done with the run.
    a2a: &'a mut A2aStream,
}

impl<'a> ProcessEventSink<'a> {
    pub(super) fn new(
        workdir: &Path,
        max_turns: u32,
        session: RunSession,
        spend: Arc<SpendMeter>,
        a2a: &'a mut A2aStream,
    ) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            max_turns,
            turns: 0,
            open: None,
            pending: HashMap::new(),
            finished: false,
            session,
            reported_session: None,
            usage: UsageCounts::default(),
            attributed: UsageCounts::default(),
            spend,
            produced: false,
            session_gone: false,
            a2a,
        }
    }

    /// The internal session workdir this run writes `out/result.txt` into.
    pub(super) fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// The runtime is about to interrupt the harness. Everything the harness says from here on is
    /// still recorded — a partial answer is worth keeping — but the attempt ends `canceled`.
    pub(super) fn mark_interrupted(&mut self) {
        self.a2a.mark_interrupted();
    }

    /// The interrupted harness had to be killed rather than ending when it was asked.
    pub(super) fn mark_harness_killed(&mut self) {
        self.a2a.mark_harness_killed();
    }

    /// The turn a record written now belongs to: the open turn, or the last one that closed.
    pub(super) fn current_turn(&self) -> u32 {
        match self.open.as_ref() {
            Some(open) => open.index,
            None => self.turns.saturating_sub(1),
        }
    }

    /// Feed one `parse` batch through. Events after a terminal one, in the same batch, are
    /// ignored: the run is over and the harness is already being shut down.
    pub(super) async fn consume(
        &mut self,
        events: Vec<Event>,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) -> SinkOutcome {
        for event in events {
            if self.finished {
                break;
            }
            match self.handle(event, hooks, trace, otel).await {
                SinkOutcome::Continue => {}
                terminal => {
                    self.finished = true;
                    return terminal;
                }
            }
        }
        SinkOutcome::Continue
    }

    /// Close the run out: commit its session, and record every tool call the harness never
    /// answered.
    ///
    /// The one place a run writes the harness session map, and the reason it is here rather than
    /// in an event arm: a session is worth remembering once the run it belongs to has produced
    /// something, and that is only known when the run is over. A run that produced nothing
    /// commits nothing, so the next turn in that context starts a conversation the harness will
    /// answer to instead of resuming one it never opened.
    ///
    /// A call with no result is not a `tool_call` record: nothing is known about how it went, and
    /// inventing a status would put a tool call in the trace that no tool ever finished.
    ///
    /// Returns the id the harness will answer to, on exactly the condition the session is
    /// committed, and `None` otherwise: that is the session a reopened attempt of the same task
    /// resumes, whether or not the context remembers it.
    pub(super) async fn finish(&mut self, trace: &mut TraceWriter) -> Option<String> {
        let continuable = (self.produced && !self.session_gone).then(|| {
            self.session.commit(self.reported_session.as_deref());
            self.session
                .effective_id(self.reported_session.as_deref())
                .to_string()
        });
        let mut unanswered: Vec<(String, String)> = self
            .pending
            .drain()
            .map(|(id, call)| (id, call.name))
            .collect();
        unanswered.sort();
        for (id, name) in unanswered {
            let _ = trace
                .write_harness_note(&format!(
                    "the harness called tool '{name}' (id {id}) and the run ended with no result \
                     for it"
                ))
                .await;
        }
        continuable
    }

    async fn handle(
        &mut self,
        event: Event,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) -> SinkOutcome {
        self.produced |= produces_observable_work(&event);
        match event {
            Event::SessionStarted(info) => {
                let _ = trace
                    .write_harness_session(&info.id, &info.auth, info.model.as_deref())
                    .await;
                if !info.id.is_empty() {
                    self.reported_session = Some(info.id.clone());
                }
                if info.auth != SUBSCRIPTION_AUTH {
                    let message = format!(
                        "the harness session reports auth '{}', not '{SUBSCRIPTION_AUTH}' — this \
                         run's spend may be billed to an API key rather than to the subscription \
                         this transport exists for",
                        info.auth
                    );
                    crate::runtime_err!(
                        "[capsule-runtime] warning[{W_SEC_031}]: {message} ({})",
                        security_warning_link(W_SEC_031)
                    );
                    let _ = trace.write_harness_warning(W_SEC_031, &message).await;
                }
                SinkOutcome::Continue
            }
            Event::Text(text) => {
                if let Some(exceeded) = self.open_turn(trace).await {
                    return exceeded;
                }
                self.a2a.open_segment(self.segment_turn()).await;
                self.a2a.complete_text();
                if let Some(open) = self.open.as_mut() {
                    open.text = Some(text);
                }
                SinkOutcome::Continue
            }
            Event::ToolCall(call) => {
                if let Some(exceeded) = self.open_turn(trace).await {
                    return exceeded;
                }
                // No frame of its own: the http path emits nothing for a tool request either.
                self.a2a.open_segment(self.segment_turn()).await;
                let Some(open) = self.open.as_mut() else {
                    return SinkOutcome::Continue;
                };
                if open.first_tool.is_none() {
                    open.first_tool = Some(call.name.clone());
                }
                let input: Value = serde_json::from_str(&call.input).unwrap_or(Value::Null);
                self.pending.insert(
                    call.id,
                    PendingToolCall {
                        name: call.name,
                        input,
                        input_bytes: call.input.len() as u64,
                        turn: open.index,
                        started: Instant::now(),
                    },
                );
                SinkOutcome::Continue
            }
            Event::ToolResult(result) => {
                // The turn that issued the call closes here, before the call is recorded, so the
                // trace reads in the order the work happened: the model decided, then the tool ran.
                // A ceiling reached by that turn stops the run before the tool call is recorded:
                // the tool has already run, and its record belongs to a run that is continuing.
                if let Some(reached) = self.close_turn(hooks, trace, otel).await {
                    return reached;
                }
                let Some(call) = self.pending.remove(&result.id) else {
                    let _ = trace
                        .write_harness_note(&format!(
                            "the harness reported a result for tool call id {} with no matching \
                             call",
                            result.id
                        ))
                        .await;
                    return SinkOutcome::Continue;
                };
                let status = if result.is_error { "error" } else { "ok" };
                let output_bytes = result.output.len() as u64;
                let result_id = result.id.clone();
                let output = result.output.clone();
                // Measured by the runtime rather than taken from the harness: the bridge executes
                // the tool, and this is the only clock that saw both ends of the call.
                let duration_ms = call
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                let _ = trace
                    .write_tool_call(
                        call.turn,
                        call.name.clone(),
                        Some(result.id),
                        call.input.clone(),
                        call.input_bytes,
                        &result.output,
                        output_bytes,
                        duration_ms,
                        status.to_string(),
                        None,
                        None,
                    )
                    .await;
                hooks
                    .emit(
                        &self.workdir,
                        HookEvent::ToolCall {
                            turn: call.turn,
                            tool_name: call.name.clone(),
                            input_bytes: call.input_bytes,
                            input: call.input.to_string(),
                            output_bytes,
                            duration_ms,
                            status: status.to_string(),
                        },
                    )
                    .await;
                // Built from the generic events alone: the call's bare name, the result's own
                // output, id and failure flag, and the runtime's own measurement — the same
                // number the `tool_call` trace record above carries. The bridge hands tool output
                // back to the harness unfenced, it ran no subprocess this runtime saw the exit of,
                // and the event carries no truncation flag, so those three keys say so.
                self.a2a
                    .tool_result(StreamArtifact::tool_call(
                        call.name,
                        output,
                        None,
                        &result_id,
                        result.is_error,
                        duration_ms,
                        None,
                        false,
                    ))
                    .await;
                SinkOutcome::Continue
            }
            // Streaming fragments and reasoning. Each is evidence the harness is alive — which
            // the runner already took from the line arriving — and none is a model action, so none
            // opens a turn. Each does open an A2A segment: the http path writes its `working`
            // status before the driver call, and therefore before any chunk.
            Event::TextDelta(text) => {
                self.a2a.open_segment(self.segment_turn()).await;
                self.a2a.text_delta(&text);
                SinkOutcome::Continue
            }
            Event::ThinkingDelta(text) => {
                self.a2a.open_segment(self.segment_turn()).await;
                self.a2a.thinking_delta(&text);
                SinkOutcome::Continue
            }
            Event::Thinking(text) => {
                self.a2a.open_segment(self.segment_turn()).await;
                self.a2a.complete_thinking(&text);
                SinkOutcome::Continue
            }
            Event::Retry(retry) => {
                let _ = trace
                    .write_harness_retry(retry.attempt, &retry.reason)
                    .await;
                SinkOutcome::Continue
            }
            // Cumulative for the whole harness run, so it is held rather than added up: what a
            // turn records is the growth `close_turn` takes off it. Held member by member, so a
            // report naming only what changed leaves the rest of the last one standing.
            Event::Usage(report) => {
                self.usage.overlay(&report);
                SinkOutcome::Continue
            }
            Event::Note(text) => {
                let _ = trace.write_harness_note(&text).await;
                SinkOutcome::Continue
            }
            Event::TurnEnd(result) => {
                // A ceiling the last turn reached outranks the answer: the run stopped at the
                // operator's limit, and `out/result.txt` says so rather than holding an answer
                // the next turn would have gone past the ceiling to improve on.
                if let Some(reached) = self.close_turn(hooks, trace, otel).await {
                    return reached;
                }
                match super::record_result(hooks, &self.workdir, &result) {
                    Ok(()) => {
                        self.a2a.turn_end(&result);
                        SinkOutcome::Ended
                    }
                    Err(message) => SinkOutcome::Failed(RuntimeError::AgentLoopFailed(message)),
                }
            }
            Event::TurnFailed(failure) => {
                // The failure is the run's own account of itself and outranks a ceiling the same
                // turn reached: the harness said why it stopped, and that is what is reported.
                let _ = self.close_turn(hooks, trace, otel).await;
                self.a2a.turn_failed();
                let kind = failure_kind_name(failure.kind);
                let _ = trace
                    .write_harness_failed(kind, &failure.message, "harness")
                    .await;
                // A turn launched `mode: resume` that failed without the harness ever reporting
                // the session it was handed is the harness saying it does not hold the
                // conversation this context names, which is its own failure — and leaves the
                // entry naming it exactly where it was.
                if let Some(gone) = self.session.session_gone(
                    failure.kind,
                    self.reported_session.is_some(),
                    &failure.message,
                ) {
                    self.session_gone = true;
                    return SinkOutcome::Failed(gone);
                }
                SinkOutcome::Failed(RuntimeError::HarnessTurnFailed {
                    kind: kind.to_string(),
                    message: failure.message,
                    origin: "harness".to_string(),
                })
            }
        }
    }

    /// The number, 1-based, that the turn counter would record a turn opening now under — which
    /// is the number the A2A segment opening with it carries. Reads the counter and never
    /// advances it: a segment is not a turn, and must not spend `max_turns` budget.
    fn segment_turn(&self) -> u32 {
        match self.open.as_ref() {
            Some(open) => open.index + 1,
            None => self.turns + 1,
        }
    }

    /// Open a turn if none is open. Returns `Some` when doing so would exceed the attempt's
    /// budget, in which case no turn is opened and the run is over.
    async fn open_turn(&mut self, trace: &mut TraceWriter) -> Option<SinkOutcome> {
        if self.open.is_some() {
            return None;
        }
        self.turns += 1;
        if self.turns > self.max_turns {
            let message = format!(
                "the harness opened turn {} with {} left in this attempt's inference.max_turns \
                 budget",
                self.turns, self.max_turns
            );
            let _ = trace
                .write_harness_failed("max-turns", &message, "runtime")
                .await;
            return Some(SinkOutcome::TurnBudgetExceeded(
                RuntimeError::HarnessTurnFailed {
                    kind: "max-turns".to_string(),
                    message,
                    origin: "runtime".to_string(),
                },
            ));
        }
        self.open = Some(OpenTurn {
            index: self.turns - 1,
            text: None,
            first_tool: None,
        });
        None
    }

    /// Write the open turn's `inference` record, if one is open, charge what it cost, and say
    /// whether that took the session to a spend ceiling.
    ///
    /// The counts are the harness's own, as its driver reported them, and this transport has no
    /// second measurement: the growth of the cumulative `usage` report goes into the record's
    /// `input_tokens`/`output_tokens` — which is what the task and session totals accumulate —
    /// and `input_tokens_actual`/`output_tokens_actual` stay absent. Cache read, cache creation
    /// and thinking ride along in a [`DriverUsage`] carrying nothing else. A driver that reported
    /// no usage leaves each count absent, which the trace keeps distinct from a count of zero.
    ///
    /// The hook is handed `0` where the trace holds absent: `murmur:hook`'s `inference-event`
    /// declares both counts as plain `u64` with no optional form, and widening them would break
    /// every published hook artifact. The absent/zero distinction lives in the trace.
    async fn close_turn(
        &mut self,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) -> Option<SinkOutcome> {
        let open = self.open.take()?;
        let decision = if open.first_tool.is_some() {
            "tool_call"
        } else {
            "end_turn"
        };
        let spent = self.usage.take_growth(&mut self.attributed);
        // `None` throughout rather than a record of absences, so an unreporting driver's turn
        // carries no provider keys at all.
        let reported = (spent != UsageCounts::default()).then_some(DriverUsage {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: spent.cache_read,
            cache_write_tokens: spent.cache_creation,
            thinking_tokens: spent.thinking,
        });
        let _ = trace
            .write_inference(
                open.index,
                spent.input,
                spent.output,
                decision.to_string(),
                // The runtime reads no driver response of its own on this transport, so there is
                // no provider stop reason and no request payload to hash.
                None,
                open.first_tool.clone(),
                None,
                reported.as_ref(),
                Vec::new(),
                None,
            )
            .await;
        otel.emit_inference(
            open.index,
            spent.input,
            spent.output,
            decision,
            None,
            open.first_tool.as_deref(),
            0,
            None,
            reported.as_ref(),
        )
        .await;
        hooks
            .emit(
                &self.workdir,
                HookEvent::Inference {
                    turn: open.index,
                    input_tokens: spent.input.unwrap_or(0),
                    output_tokens: spent.output.unwrap_or(0),
                    decision: decision.to_string(),
                    // The same value the trace records, so a hook bound to `on-inference` reads
                    // the same turn on either transport.
                    tool_name: open.first_tool.clone(),
                    prompt: None,
                    output: open.text,
                    tools: None,
                },
            )
            .await;
        // Charged after the record is written, so what the meter holds is always a sum of lines
        // already in the trace. Cache and thinking are not charged: the meter's definition is
        // `input + output` on both transports.
        self.spend
            .charge_spent(spent.input.unwrap_or(0), spent.output.unwrap_or(0));
        let refusal = self.spend.ceiling_crossed()?;
        let _ = trace
            .write_spend_ceiling_reached(open.index, &refusal, None)
            .await;
        self.a2a.mark_spend_refused(&refusal);
        Some(SinkOutcome::SpendCeilingReached(refusal))
    }
}

/// Whether an event is work the harness actually produced, which is the whole of what makes a
/// run's session id worth keeping.
///
/// Six events say yes: a complete or streamed piece of text, a complete or streamed thought, a
/// tool call, and a turn that ended. `turn-end` counts whatever its result text says — a turn that
/// ran to completion is a conversation the harness holds, however little it had to say.
///
/// The rest say no, and `session-started` is why this predicate exists: a harness prints the name
/// of its session before the model has produced anything and before it has written a transcript,
/// so a run that got no further leaves the harness with nothing to answer to under that name. A
/// retry, a note, a tool result and a failed turn each report on work rather than being it.
fn produces_observable_work(event: &Event) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::TextDelta(_)
            | Event::Thinking(_)
            | Event::ThinkingDelta(_)
            | Event::ToolCall(_)
            | Event::TurnEnd(_)
    )
}

/// The WIT spelling of a `failure-kind`, which is what the error and the trace both name.
pub(super) fn failure_kind_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Auth => "auth",
        FailureKind::Quota => "quota",
        FailureKind::MaxTurns => "max-turns",
        FailureKind::Canceled => "canceled",
        FailureKind::HarnessError => "harness-error",
        FailureKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    use super::*;
    use crate::agent::process::{plan_session, HarnessSessionPolicy, SessionPlan};
    use crate::harness_session::HarnessSessionMap;
    use crate::process_driver::{
        RetryInfo, SessionInfo, ToolCallInfo, ToolResultInfo, TurnFailure,
    };
    use crate::streaming::SseEventBuffer;
    use serde_json::Value as Json;

    /// The context every sink below runs a task under.
    const CONTEXT: &str = "ctx_test";

    /// The harness name every sink below records with the session it remembers.
    const HARNESS: &str = "fixture-harness";

    /// The label a frame reads as: its event type, and for the two types whose shape depends on a
    /// key, the key. The same labelling the process/http parity tests compare sequences with.
    fn frame_kind(frame: &str) -> String {
        let data: Json = serde_json::from_str(frame_data(frame)).expect("frame data is JSON");
        match event_type(frame) {
            "status" => match data["status"]["state"].as_str().unwrap_or_default() {
                "working" => format!(
                    "status:working:{}",
                    data["status"]["message"].as_str().unwrap_or_default()
                ),
                state => format!("status:{state}"),
            },
            "text" => match data["final"] == Json::Bool(true) {
                true => "text:final".to_string(),
                false => "text:chunk".to_string(),
            },
            other => other.to_string(),
        }
    }

    fn event_type(frame: &str) -> &str {
        frame
            .lines()
            .find_map(|line| line.strip_prefix("event: "))
            .expect("every frame names its event type")
    }

    fn frame_data(frame: &str) -> &str {
        frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("every frame carries data")
    }

    /// The parsed `data` of every frame of `event_type`, in order.
    fn data_of(frames: &[String], event_type_wanted: &str) -> Vec<Json> {
        frames
            .iter()
            .filter(|frame| event_type(frame) == event_type_wanted)
            .map(|frame| serde_json::from_str(frame_data(frame)).expect("frame data is JSON"))
            .collect()
    }

    /// A sink's sinks and the workdir they write into. Held together so a test can drop the
    /// tempdir only after reading `trace.jsonl` back, and so one A2A stream spans every batch a
    /// test feeds.
    struct Harness {
        _dir: tempfile::TempDir,
        workdir: PathBuf,
        max_turns: u32,
        /// The session policy and plan this harness runs under. Held rather than the `RunSession`
        /// itself because [`ProcessEventSink::new`] takes one by value, and the sink is rebuilt
        /// per batch; planning once keeps `planned_id` the id every batch is handed.
        policy: HarnessSessionPolicy,
        plan: SessionPlan,
        /// The spend account every batch's sink shares, so a ceiling test can hold one across
        /// several batches the way one run does.
        spend: Arc<SpendMeter>,
        a2a: A2aStream,
        /// Every frame the A2A stream wrote, in order, as the session buffer recorded them.
        frames: Arc<Mutex<SseEventBuffer>>,
        hooks: HookRuntime,
        trace: TraceWriter,
        otel: OtelEmitter,
        /// The map the sink writes back into, so a test can read what the run remembered.
        map: Arc<HarnessSessionMap>,
        /// The id the runtime handed the harness for this run.
        planned_id: String,
        /// What the last batch's `finish` handed back for a reopened attempt to resume.
        carried: Option<String>,
    }

    impl Harness {
        async fn new(max_turns: u32) -> Self {
            Self::threading(
                max_turns,
                Arc::new(HarnessSessionMap::new(None, Path::new("/tmp"))),
            )
            .await
        }

        /// The same harness, metered by `spend` rather than by an unlimited meter.
        fn metered(mut self, spend: SpendMeter) -> Self {
            self.spend = Arc::new(spend);
            self
        }

        /// A sink threading `CONTEXT` through `map`: `mode: resume` when the map already holds an
        /// id for it, `mode: new` otherwise.
        async fn threading(max_turns: u32, map: Arc<HarnessSessionMap>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let workdir = dir.path().to_path_buf();
            std::fs::create_dir_all(workdir.join("tools")).unwrap();
            let engine = crate::runtime::build_engine().unwrap();
            let hooks = HookRuntime::new(
                &engine,
                &workdir,
                &workdir,
                Vec::new(),
                crate::hooks::SessionContextData {
                    capsule_name: "cap".to_string(),
                    capsule_version: "0.1.0".to_string(),
                    session_id: "ses_test".to_string(),
                    model: "test-model".to_string(),
                    capabilities: Vec::new(),
                },
                crate::hooks::HookEnvVars::default(),
                crate::limits::ExecutionLimits::default(),
                None,
                None,
            )
            .await
            .unwrap();
            let trace = TraceWriter::open(
                &workdir,
                "ses_test".to_string(),
                "cap".to_string(),
                "0.1.0".to_string(),
                "test-model".to_string(),
                Vec::new(),
                crate::containment::scope_report_for_tier(
                    &crate::CapabilityPolicy::default(),
                    murmur_artifact::ContainmentClass::Advisory,
                    crate::sandbox::EnforcementTier::EnvironmentOnly,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    crate::cgroup::IoMaxReport::default(),
                ),
                murmur_artifact::TraceCapture::Meta,
                None,
                false,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let otel = OtelEmitter::new(None, &workdir, "cap".to_string(), "0.1.0".to_string());
            let policy = HarnessSessionPolicy {
                map: Arc::clone(&map),
                context_id: Some(CONTEXT.to_string()),
                continue_conversation: true,
                forget: None,
                reopen_session: None,
            };
            let plan = plan_session(&policy);
            let planned_id = plan.session.id.clone();
            // A real broadcast and buffer, so the frames these tests read are the bytes a client
            // would have received. The buffer records them whether or not a receiver is attached,
            // which is what these tests read back.
            let (tx, _rx) = tokio::sync::broadcast::channel(256);
            let frames = Arc::new(Mutex::new(SseEventBuffer::new(256)));
            Self {
                _dir: dir,
                workdir: workdir.clone(),
                max_turns,
                policy,
                plan,
                spend: Arc::new(SpendMeter::unlimited()),
                a2a: A2aStream::new(
                    Some((tx, Arc::clone(&frames))),
                    Some("tsk_test".to_string()),
                    Some(CONTEXT.to_string()),
                    Arc::new(AtomicBool::new(false)),
                ),
                frames,
                hooks,
                trace,
                otel,
                map,
                planned_id,
                carried: None,
            }
        }

        /// The sink is built per batch because it borrows the stream; its turn state is a
        /// test-local concern and every test here feeds exactly one batch.
        async fn feed(&mut self, events: Vec<Event>) -> SinkOutcome {
            let session = RunSession::new(&self.plan, &self.policy, HARNESS, "fixture-driver");
            let mut sink = ProcessEventSink::new(
                &self.workdir,
                self.max_turns,
                session,
                Arc::clone(&self.spend),
                &mut self.a2a,
            );
            let outcome = sink
                .consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await;
            self.carried = sink.finish(&mut self.trace).await;
            outcome
        }

        /// Each frame the A2A stream wrote, labelled by kind, in order.
        fn frame_kinds(&self) -> Vec<String> {
            self.frames()
                .iter()
                .map(|frame| frame_kind(frame))
                .collect()
        }

        /// Each frame's `event: <type>` and parsed `data`, in order.
        fn frames(&self) -> Vec<String> {
            match self.frames.lock().unwrap().replay_from(0) {
                crate::streaming::ReplayResult::Complete(frames)
                | crate::streaming::ReplayResult::WithGap { events: frames, .. } => {
                    frames.iter().map(|frame| frame.to_string()).collect()
                }
            }
        }

        /// Every event written so far, parsed.
        async fn events(&mut self) -> Vec<Json> {
            self.trace.flush().await.unwrap();
            std::fs::read_to_string(self.workdir.join("trace.jsonl"))
                .unwrap()
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        async fn of_type(&mut self, event_type: &str) -> Vec<Json> {
            self.events()
                .await
                .into_iter()
                .filter(|event| event["event_type"] == event_type)
                .collect()
        }
    }

    fn text(t: &str) -> Event {
        Event::Text(t.to_string())
    }

    fn tool_call(id: &str, name: &str) -> Event {
        Event::ToolCall(ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: r#"{"msg":"hi"}"#.to_string(),
        })
    }

    fn tool_result(id: &str, is_error: bool) -> Event {
        Event::ToolResult(ToolResultInfo {
            id: id.to_string(),
            output: "done".to_string(),
            is_error,
        })
    }

    /// A `usage` report naming only some members, with the rest of the last one standing.
    fn usage(members: &[(&str, u64)]) -> Event {
        let mut report = Usage {
            input: None,
            output: None,
            cache_read: None,
            cache_creation: None,
            thinking: None,
        };
        for (member, value) in members {
            let slot = match *member {
                "in" => &mut report.input,
                "out" => &mut report.output,
                "cache-read" => &mut report.cache_read,
                "cache-creation" => &mut report.cache_creation,
                "thinking" => &mut report.thinking,
                other => panic!("no such usage member: {other}"),
            };
            *slot = Some(*value);
        }
        Event::Usage(report)
    }

    /// The reported counts land in `input_tokens`/`output_tokens` — which is what the totals
    /// accumulate — and the cache and thinking counts sit beside them. `*_actual` stays absent:
    /// there is one measurement on this transport and it is recorded once.
    #[tokio::test]
    async fn usage_a_reported_turn_carries_the_harness_counts() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            usage(&[
                ("in", 120),
                ("out", 30),
                ("cache-read", 900),
                ("cache-creation", 64),
                ("thinking", 2),
            ]),
            text("answer"),
            Event::TurnEnd("answer".into()),
        ])
        .await;
        let inference = &h.of_type("inference").await[0];
        assert_eq!(inference["input_tokens"], 120);
        assert_eq!(inference["output_tokens"], 30);
        assert_eq!(inference["cached_tokens"], 900);
        assert_eq!(inference["cache_write_tokens"], 64);
        assert_eq!(inference["thinking_tokens"], 2);
        assert!(
            inference.get("input_tokens_actual").is_none(),
            "{inference}"
        );
        assert!(
            inference.get("output_tokens_actual").is_none(),
            "{inference}"
        );
    }

    /// A driver that reported nothing leaves both counts off the record entirely, which is a
    /// different fact from a harness that reported spending none.
    #[tokio::test]
    async fn usage_an_unreported_turn_carries_no_counts_at_all() {
        let mut h = Harness::new(10).await;
        h.feed(vec![text("answer"), Event::TurnEnd("answer".into())])
            .await;
        let inference = &h.of_type("inference").await[0];
        assert!(inference.get("input_tokens").is_none(), "{inference}");
        assert!(inference.get("output_tokens").is_none(), "{inference}");
        assert!(inference.get("thinking_tokens").is_none(), "{inference}");
    }

    #[tokio::test]
    async fn usage_a_reported_zero_is_written_as_zero() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            usage(&[("in", 0), ("out", 0)]),
            text("answer"),
            Event::TurnEnd("answer".into()),
        ])
        .await;
        let inference = &h.of_type("inference").await[0];
        assert_eq!(inference["input_tokens"], 0);
        assert_eq!(inference["output_tokens"], 0);
    }

    /// Every report is the total for the run, so the second turn is charged the growth alone and
    /// the two turns together add up to the last report rather than to twice it.
    #[tokio::test]
    async fn usage_is_cumulative_so_each_turn_records_only_its_growth() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            usage(&[("in", 40), ("out", 20)]),
            tool_call("c1", "t"),
            tool_result("c1", false),
            usage(&[("in", 100), ("out", 50)]),
            text("two"),
            Event::TurnEnd("two".into()),
        ])
        .await;
        let inferences = h.of_type("inference").await;
        assert_eq!(inferences.len(), 2);
        assert_eq!(inferences[0]["input_tokens"], 40);
        assert_eq!(inferences[0]["output_tokens"], 20);
        assert_eq!(inferences[1]["input_tokens"], 60);
        assert_eq!(inferences[1]["output_tokens"], 30);
    }

    /// The same total reported twice adds nothing, and a member that shrinks contributes zero
    /// rather than wrapping or subtracting.
    #[tokio::test]
    async fn usage_repeated_and_shrinking_reports_add_nothing() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            usage(&[("in", 100), ("out", 50)]),
            tool_call("c1", "t"),
            tool_result("c1", false),
            usage(&[("in", 100), ("out", 10)]),
            text("two"),
            Event::TurnEnd("two".into()),
        ])
        .await;
        let inferences = h.of_type("inference").await;
        assert_eq!(inferences[1]["input_tokens"], 0);
        assert_eq!(inferences[1]["output_tokens"], 0);
    }

    /// A member a later report omits keeps the value the earlier one gave it, rather than being
    /// retracted and counted again from zero.
    #[tokio::test]
    async fn usage_a_member_a_later_report_omits_keeps_standing() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            usage(&[("in", 100), ("cache-read", 7)]),
            tool_call("c1", "t"),
            tool_result("c1", false),
            usage(&[("out", 5)]),
            text("two"),
            Event::TurnEnd("two".into()),
        ])
        .await;
        let inferences = h.of_type("inference").await;
        assert_eq!(inferences[0]["input_tokens"], 100);
        assert_eq!(inferences[0]["cached_tokens"], 7);
        assert_eq!(inferences[1]["input_tokens"], 0);
        assert_eq!(inferences[1]["output_tokens"], 5);
        // Reported once means reported from then on: the second turn's growth over the same
        // total is zero, which is a count, not an absence.
        assert_eq!(inferences[1]["cached_tokens"], 0);
    }

    /// The meter is charged `input + output` alone, and a turn that reaches the ceiling ends
    /// the run with the refusal and its `spend_ceiling_reached` record.
    #[tokio::test]
    async fn usage_reaches_the_spend_meter_and_crosses_its_ceiling() {
        let mut h = Harness::new(10)
            .await
            .metered(SpendMeter::new(Some(100), None));
        let outcome = h
            .feed(vec![
                usage(&[("in", 80), ("out", 40), ("cache-read", 9_000)]),
                text("answer"),
                Event::TurnEnd("answer".into()),
            ])
            .await;
        let refusal = match outcome {
            SinkOutcome::SpendCeilingReached(refusal) => refusal,
            _ => panic!("a turn past the ceiling must end the run"),
        };
        // Cache reads are never folded into what the meter charges: 80 + 40 crosses 100, and the
        // 9000 cached prompt tokens are beside it rather than inside it.
        assert_eq!(refusal.used, 120);
        assert_eq!(refusal.requested, 0);
        let reached = h.of_type("spend_ceiling_reached").await;
        assert_eq!(reached.len(), 1);
        assert_eq!(reached[0]["limit"], "session");
        assert!(
            refusal
                .to_string()
                .contains("inference.max_session_tokens is 100"),
            "{refusal}"
        );
    }

    /// A run under the ceiling ends the way it would with no ceiling at all.
    #[tokio::test]
    async fn usage_a_run_under_the_ceiling_is_untouched() {
        let mut h = Harness::new(10)
            .await
            .metered(SpendMeter::new(Some(1_000), None));
        let outcome = h
            .feed(vec![
                usage(&[("in", 80), ("out", 40)]),
                text("answer"),
                Event::TurnEnd("answer".into()),
            ])
            .await;
        assert!(matches!(outcome, SinkOutcome::Ended));
        assert!(h.of_type("spend_ceiling_reached").await.is_empty());
    }

    #[tokio::test]
    async fn one_text_and_a_terminal_event_are_one_turn() {
        let mut h = Harness::new(10).await;
        assert!(matches!(
            h.feed(vec![text("hello"), Event::TurnEnd("hello".into())])
                .await,
            SinkOutcome::Ended
        ));
        let inferences = h.of_type("inference").await;
        assert_eq!(inferences.len(), 1);
        assert_eq!(inferences[0]["decision"], "end_turn");
        assert_eq!(
            std::fs::read_to_string(h.workdir.join("out/result.txt")).unwrap(),
            "hello"
        );
    }

    /// A tool result closes the turn that issued the call, and the next action opens a new one.
    #[tokio::test]
    async fn a_tool_result_closes_its_turn_and_the_next_action_opens_another() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            text("all done"),
            Event::TurnEnd("all done".into()),
        ])
        .await;
        let inferences = h.of_type("inference").await;
        assert_eq!(inferences.len(), 2, "{inferences:#?}");
        assert_eq!(inferences[0]["decision"], "tool_call");
        assert_eq!(inferences[0]["tool_name"], "echo-tool");
        assert_eq!(inferences[0]["turn"], 0);
        assert_eq!(inferences[1]["decision"], "end_turn");
        assert_eq!(inferences[1]["turn"], 1);

        let calls = h.of_type("tool_call").await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["tool_name"], "echo-tool");
        assert_eq!(calls[0]["tool_call_id"], "c1");
        assert_eq!(calls[0]["status"], "ok");
        assert_eq!(calls[0]["turn"], 0);
    }

    #[tokio::test]
    async fn an_error_result_is_recorded_as_one() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", true),
            Event::TurnEnd(String::new()),
        ])
        .await;
        assert_eq!(h.of_type("tool_call").await[0]["status"], "error");
    }

    /// Thinking and the streaming deltas are not model actions: a run of nothing but those, then
    /// one `text`, is one turn.
    #[tokio::test]
    async fn thinking_and_deltas_never_open_a_turn() {
        let mut h = Harness::new(1).await;
        let outcome = h
            .feed(vec![
                Event::Thinking("pondering".into()),
                Event::ThinkingDelta("more".into()),
                Event::TextDelta("par".into()),
                text("answer"),
                Event::TurnEnd("answer".into()),
            ])
            .await;
        assert!(matches!(outcome, SinkOutcome::Ended));
        assert_eq!(h.of_type("inference").await.len(), 1);
    }

    #[tokio::test]
    async fn a_turn_past_the_budget_ends_the_run() {
        let mut h = Harness::new(1).await;
        let outcome = h
            .feed(vec![
                tool_call("c1", "t"),
                tool_result("c1", false),
                text("a second turn"),
            ])
            .await;
        match outcome {
            SinkOutcome::TurnBudgetExceeded(RuntimeError::HarnessTurnFailed {
                kind,
                origin,
                ..
            }) => {
                assert_eq!(kind, "max-turns");
                assert_eq!(origin, "runtime");
            }
            _ => panic!("a turn past the budget must end the run"),
        }
        assert_eq!(h.of_type("inference").await.len(), 1);
        let failed = h.of_type("harness_failed").await;
        assert_eq!(failed[0]["kind"], "max-turns");
        assert_eq!(failed[0]["source"], "runtime");
    }

    #[tokio::test]
    async fn a_failed_turn_fails_the_run_and_names_its_kind() {
        let mut h = Harness::new(10).await;
        let outcome = h
            .feed(vec![
                text("partial"),
                Event::TurnFailed(TurnFailure {
                    kind: FailureKind::Auth,
                    message: "not signed in".into(),
                }),
            ])
            .await;
        match outcome {
            SinkOutcome::Failed(RuntimeError::HarnessTurnFailed {
                kind,
                message,
                origin,
            }) => {
                assert_eq!(kind, "auth");
                assert_eq!(message, "not signed in");
                assert_eq!(origin, "harness");
            }
            _ => panic!("a turn-failed event must fail the run"),
        }
        assert_eq!(
            h.of_type("inference").await.len(),
            1,
            "the open turn is closed before the failure"
        );
        assert_eq!(h.of_type("harness_failed").await[0]["source"], "harness");
        assert!(
            !h.workdir.join("out/result.txt").exists(),
            "a failed turn produces no result"
        );
    }

    /// Nothing after the first terminal event in a batch is acted on: the run is over.
    #[tokio::test]
    async fn events_after_a_terminal_event_are_ignored() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            text("first"),
            Event::TurnEnd("first".into()),
            text("second"),
            Event::TurnEnd("second".into()),
        ])
        .await;
        assert_eq!(h.of_type("inference").await.len(), 1);
        assert_eq!(
            std::fs::read_to_string(h.workdir.join("out/result.txt")).unwrap(),
            "first"
        );
    }

    #[tokio::test]
    async fn a_non_subscription_session_is_warned_about_and_traced() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::SessionStarted(SessionInfo {
            id: "harness-session-9".into(),
            auth: "api-key".into(),
            model: Some("a-model".into()),
        })])
        .await;
        let sessions = h.of_type("harness_session").await;
        assert_eq!(sessions[0]["harness_session_id"], "harness-session-9");
        assert_eq!(sessions[0]["auth"], "api-key");
        assert_eq!(sessions[0]["model"], "a-model");
        let warnings = h.of_type("harness_warning").await;
        assert_eq!(warnings[0]["code"], W_SEC_031);
        assert!(warnings[0]["message"].as_str().unwrap().contains("api-key"));
    }

    #[tokio::test]
    async fn a_subscription_session_raises_no_warning() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::SessionStarted(SessionInfo {
            id: "s".into(),
            auth: SUBSCRIPTION_AUTH.into(),
            model: None,
        })])
        .await;
        assert_eq!(h.of_type("harness_session").await.len(), 1);
        assert!(h.of_type("harness_warning").await.is_empty());
    }

    #[tokio::test]
    async fn retries_and_notes_are_traced() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::Retry(RetryInfo {
                attempt: 2,
                reason: "overloaded".into(),
            }),
            Event::Note("a line the driver could not read".into()),
        ])
        .await;
        let retries = h.of_type("harness_retry").await;
        assert_eq!(retries[0]["attempt"], 2);
        assert_eq!(retries[0]["reason"], "overloaded");
        assert_eq!(
            h.of_type("harness_note").await[0]["text"],
            "a line the driver could not read"
        );
    }

    // ── The A2A frames one attempt writes ─────────────────────────────────────

    /// A segment that streams fragments and then reports the complete text sends the client one
    /// cursor removal and never the words twice: on this wire a `text` replaces, and the client
    /// keeps what it was streamed.
    #[tokio::test]
    async fn a_streamed_segment_sends_one_cursor_removal() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::TextDelta("one ".into()),
            Event::TextDelta("two".into()),
            text("one two"),
            Event::TurnEnd("one two".into()),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            vec![
                "status:working:inference turn 1",
                "text:chunk",
                "text:chunk",
                "text:final",
            ],
            "{:#?}",
            h.frames()
        );
        let texts = data_of(&h.frames(), "text");
        assert_eq!(texts[0]["text"], "one ");
        assert_eq!(texts[1]["text"], "two");
        assert_eq!(texts[2]["text"], "", "the cursor removal carries no text");
        assert_eq!(texts[2]["final"], true);
    }

    /// The cursor removal a segment owes is paid before the artifact frame that closes it, so a
    /// client's streamed item is complete before the tool result lands under it.
    #[tokio::test]
    async fn a_cursor_removal_precedes_the_artifact_that_closes_its_segment() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::TextDelta("thinking about it".into()),
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            text("done"),
            Event::TurnEnd("done".into()),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            vec![
                "status:working:inference turn 1",
                "text:chunk",
                "text:final",
                "artifact",
                "status:working:inference turn 2",
                "text:final",
            ],
            "{:#?}",
            h.frames()
        );
    }

    /// Every key of the artifact frame comes from the contract's own events, or from the
    /// runtime's own measurement of the call.
    #[tokio::test]
    async fn the_artifact_frame_is_built_from_the_generic_events() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", true),
            Event::TurnEnd(String::new()),
        ])
        .await;
        let frames = h.frames();
        let artifact = &data_of(&frames, "artifact")[0]["artifact"];
        assert_eq!(artifact["tool_name"], "echo-tool");
        assert_eq!(artifact["content"], "done");
        assert_eq!(artifact["tool_call_id"], "c1");
        assert_eq!(artifact["is_error"], true);
        assert!(artifact["duration_ms"].is_number(), "{artifact}");
        assert_eq!(
            artifact["fence_source"],
            Json::Null,
            "the bridge returns tool output unfenced"
        );
        assert_eq!(artifact["exit_code"], Json::Null);
        assert_eq!(artifact["truncated"], false);
    }

    /// A tool result nothing called for writes no frame, the same way it writes no `tool_call`
    /// record.
    #[tokio::test]
    async fn an_unmatched_tool_result_writes_no_frame() {
        let mut h = Harness::new(10).await;
        h.feed(vec![tool_result("ghost", false)]).await;
        assert!(h.frame_kinds().is_empty(), "{:#?}", h.frames());
    }

    /// A segment that streamed the client its text does not send it again when the turn ends.
    #[tokio::test]
    async fn a_turn_end_after_streamed_text_sends_no_fallback() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::TextDelta("all of it".into()),
            Event::TurnEnd("all of it".into()),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            vec![
                "status:working:inference turn 1",
                "text:chunk",
                "text:final"
            ],
            "{:#?}",
            h.frames()
        );
        assert_eq!(
            data_of(&h.frames(), "text")[1]["text"],
            "",
            "the one final text frame is the cursor removal, not the result again"
        );
    }

    /// A segment that streamed nothing sends the whole result as one final text frame — what the
    /// http path does for a driver that emits no chunks.
    #[tokio::test]
    async fn a_turn_end_after_an_unstreamed_segment_sends_the_result() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            text("the answer"),
            Event::TurnEnd("the answer".into()),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            vec!["status:working:inference turn 1", "text:final"],
            "{:#?}",
            h.frames()
        );
        assert_eq!(data_of(&h.frames(), "text")[0]["text"], "the answer");
    }

    /// An empty result is no answer to send.
    #[tokio::test]
    async fn a_turn_end_with_an_empty_result_sends_no_text() {
        let mut h = Harness::new(10).await;
        h.feed(vec![text(""), Event::TurnEnd(String::new())]).await;
        assert_eq!(
            h.frame_kinds(),
            vec!["status:working:inference turn 1"],
            "{:#?}",
            h.frames()
        );
    }

    /// Thinking reaches the client, and a complete `thinking` after fragments is not sent again:
    /// the client already has every word of it.
    #[tokio::test]
    async fn thinking_fragments_are_sent_once() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::ThinkingDelta("step ".into()),
            Event::ThinkingDelta("one".into()),
            Event::Thinking("step one".into()),
            text("done"),
            Event::TurnEnd("done".into()),
        ])
        .await;
        let frames = h.frames();
        let thinking = data_of(&frames, "thinking");
        assert_eq!(thinking.len(), 2, "{frames:#?}");
        assert_eq!(thinking[0]["text"], "step ");
        assert_eq!(thinking[1]["text"], "one");
        for frame in &thinking {
            assert_eq!(
                frame["final"], false,
                "thinking is never final on this wire"
            );
        }
    }

    /// A complete `thinking` with no fragments before it is the client's only copy, and is sent.
    #[tokio::test]
    async fn a_whole_thinking_is_sent_when_nothing_streamed_it() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::Thinking("whole thought".into()),
            text("done"),
            Event::TurnEnd("done".into()),
        ])
        .await;
        let frames = h.frames();
        let thinking = data_of(&frames, "thinking");
        assert_eq!(thinking.len(), 1, "{frames:#?}");
        assert_eq!(thinking[0]["text"], "whole thought");
        assert_eq!(thinking[0]["final"], false);
    }

    /// A provider retry, a note and the session's own start are trace records and nothing else:
    /// the http stream has no frame for any of them, and a frame here would be one the two
    /// transports do not share.
    #[tokio::test]
    async fn retries_notes_and_session_starts_write_no_frame() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::SessionStarted(SessionInfo {
                id: "s".into(),
                auth: SUBSCRIPTION_AUTH.into(),
                model: None,
            }),
            Event::Retry(RetryInfo {
                attempt: 2,
                reason: "overloaded".into(),
            }),
            Event::Note("a line the driver could not read".into()),
        ])
        .await;
        assert!(h.frame_kinds().is_empty(), "{:#?}", h.frames());
    }

    /// A segment's number is the number the turn counter would give a turn opening with it, so a
    /// fragment that opens a segment before the turn it belongs to names the same turn.
    #[tokio::test]
    async fn a_segment_carries_the_number_of_the_turn_it_opens_with() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            Event::TextDelta("second".into()),
            text("second"),
            Event::TurnEnd("second".into()),
        ])
        .await;
        let working: Vec<String> = h
            .frame_kinds()
            .into_iter()
            .filter(|kind| kind.starts_with("status:working"))
            .collect();
        assert_eq!(
            working,
            vec![
                "status:working:inference turn 1",
                "status:working:inference turn 2"
            ],
            "{:#?}",
            h.frames()
        );
        assert_eq!(
            h.of_type("inference").await.len(),
            2,
            "a segment reads the turn counter and never advances it"
        );
    }

    /// The attempt's terminal status, on the two paths out of a run the harness itself ends.
    #[tokio::test]
    async fn an_attempt_ends_with_one_terminal_status() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            text("the answer"),
            Event::TurnEnd("the answer".into()),
        ])
        .await;
        h.a2a.finish(&Ok(crate::agent::AgentLoopExit::Ok)).await;
        assert_eq!(h.frame_kinds().last().unwrap(), "status:completed");
        let completed = data_of(&h.frames(), "status").pop().unwrap();
        assert_eq!(completed["status"]["response"], "the answer");
        assert_eq!(completed["status"]["message"], "session ended");
        assert_eq!(completed["final"], true);
        assert_eq!(completed["context_id"], "ctx_test");

        let mut h = Harness::new(10).await;
        h.feed(vec![Event::TurnFailed(TurnFailure {
            kind: FailureKind::Auth,
            message: "not signed in".into(),
        })])
        .await;
        h.a2a
            .finish(&Err(RuntimeError::HarnessTurnFailed {
                kind: "auth".into(),
                message: "not signed in".into(),
                origin: "harness".into(),
            }))
            .await;
        assert_eq!(h.frame_kinds(), vec!["status:failed"], "{:#?}", h.frames());
        let failed = data_of(&h.frames(), "status").pop().unwrap();
        let message = failed["status"]["message"].as_str().unwrap();
        assert!(message.contains("E-RUN-033"), "{message}");
        assert!(message.contains("auth"), "{message}");
        assert!(message.contains("not signed in"), "{message}");
        assert_eq!(failed["final"], true);
    }

    /// Once the runtime has interrupted an attempt it ends `canceled`, whatever the harness says
    /// next: a finished turn is not a completion and a failed one — of any kind, the `canceled`
    /// kind a harness that honours an interrupt reports included — is not a failure. The answer
    /// the harness produced anyway is kept, and is not sent as the client's final text.
    #[tokio::test]
    async fn an_interrupted_attempt_ends_canceled_whatever_the_harness_said() {
        let last_words = || {
            let mut events = vec![Event::TurnEnd("the answer".into())];
            for kind in [
                FailureKind::Auth,
                FailureKind::Quota,
                FailureKind::MaxTurns,
                FailureKind::Canceled,
                FailureKind::HarnessError,
                FailureKind::Other,
            ] {
                events.push(Event::TurnFailed(TurnFailure {
                    kind,
                    message: "stopped".into(),
                }));
            }
            // And a run the harness never answered at all.
            events.push(Event::Note("nothing terminal here".into()));
            events
        };
        for last in last_words() {
            for killed in [false, true] {
                let mut h = Harness::new(10).await;
                h.a2a.mark_interrupted();
                h.feed(vec![text("the answer"), last.clone()]).await;
                if killed {
                    h.a2a.mark_harness_killed();
                }
                h.a2a
                    .finish(&Ok(crate::agent::AgentLoopExit::Canceled))
                    .await;

                let statuses = data_of(&h.frames(), "status");
                let terminal: Vec<&Json> = statuses
                    .iter()
                    .filter(|status| status["final"] == Json::Bool(true))
                    .collect();
                assert_eq!(terminal.len(), 1, "{:#?}", h.frames());
                assert_eq!(terminal[0]["status"]["state"], "canceled");
                assert_eq!(terminal[0]["context_id"], "ctx_test");
                assert!(terminal[0]["status"]["response"].is_null());
                let message = terminal[0]["status"]["message"].as_str().unwrap();
                assert_eq!(
                    message.contains("the harness was killed"),
                    killed,
                    "{message}"
                );
                assert!(
                    !h.frame_kinds().contains(&"text:final".to_string()),
                    "a stopped task is sent no final answer: {:#?}",
                    h.frames()
                );
            }
        }
    }

    /// A run with no A2A task — `mur run`, a `task.md` launch — writes no frame at all.
    #[tokio::test]
    async fn a_run_with_no_task_writes_no_frames() {
        let mut h = Harness::new(10).await;
        let frames = Arc::clone(&h.frames);
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        h.a2a = A2aStream::new(
            Some((tx, Arc::clone(&frames))),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        h.feed(vec![
            Event::TextDelta("streamed".into()),
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            text("done"),
            Event::TurnEnd("done".into()),
        ])
        .await;
        h.a2a.finish(&Ok(crate::agent::AgentLoopExit::Ok)).await;
        assert!(h.frame_kinds().is_empty(), "{:#?}", h.frames());
    }

    /// A result nobody called for, and a call nobody answered, are both notes — never a
    /// `tool_call` record for something that did not happen.
    #[tokio::test]
    async fn unpaired_calls_and_results_are_notes() {
        let mut h = Harness::new(10).await;
        h.feed(vec![tool_result("ghost", false), tool_call("c9", "t")])
            .await;
        let notes = h.of_type("harness_note").await;
        assert_eq!(notes.len(), 2, "{notes:#?}");
        assert!(notes[0]["text"].as_str().unwrap().contains("ghost"));
        assert!(notes[1]["text"].as_str().unwrap().contains("c9"));
        assert!(h.of_type("tool_call").await.is_empty());
    }

    /// The event a run names its session with. Every session test below starts from one.
    fn session_started(id: &str) -> Event {
        Event::SessionStarted(SessionInfo {
            id: id.to_string(),
            auth: SUBSCRIPTION_AUTH.into(),
            model: None,
        })
    }

    /// Whatever the harness calls its session is what the context is keyed on from then on.
    #[tokio::test]
    async fn harness_session_the_reported_id_replaces_the_one_the_runtime_minted() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            session_started("harness-chose-this"),
            text("here it is"),
            Event::TurnEnd("here it is".into()),
        ])
        .await;
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some("harness-chose-this"));
        assert_ne!(h.planned_id, "harness-chose-this");
    }

    /// The harness named its session and then the turn was interrupted before it produced
    /// anything. Nothing is remembered, so the next task in this context starts a conversation
    /// the harness will answer to.
    #[tokio::test]
    async fn harness_session_a_run_that_only_named_its_session_remembers_nothing() {
        let mut h = Harness::new(10).await;
        h.feed(vec![session_started("named-but-empty")]).await;
        assert_eq!(h.map.get(CONTEXT), None);
    }

    /// The four events that are not work: a retry, a note, a tool result nobody called for, and a
    /// failed turn leave the context exactly as they found it.
    #[tokio::test]
    async fn harness_session_reports_about_work_are_not_work() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            session_started("named-but-empty"),
            Event::Retry(RetryInfo {
                attempt: 2,
                reason: "overloaded".into(),
            }),
            Event::Note("a line the driver could not read".into()),
            tool_result("ghost", false),
            Event::TurnFailed(TurnFailure {
                kind: FailureKind::Canceled,
                message: "stopped".into(),
            }),
        ])
        .await;
        assert_eq!(h.map.get(CONTEXT), None);
    }

    /// Each of the five events that is work, on its own, with no terminal event after it: the run
    /// was interrupted past the window where the harness has a name and no conversation, and the
    /// session it established is kept.
    #[tokio::test]
    async fn harness_session_any_observable_work_commits_the_reported_id() {
        for work in [
            text("an answer"),
            Event::TextDelta("an ".into()),
            Event::Thinking("pondering".into()),
            Event::ThinkingDelta("pond".into()),
            tool_call("c1", "echo-tool"),
        ] {
            let mut h = Harness::new(10).await;
            h.feed(vec![session_started("worked"), work]).await;
            assert_eq!(h.map.get(CONTEXT).as_deref(), Some("worked"));
        }
    }

    /// A harness that says nothing about its session answers to the id it was handed.
    #[tokio::test]
    async fn harness_session_a_silent_run_that_ends_remembers_the_minted_id() {
        let mut h = Harness::new(10).await;
        assert_eq!(h.map.get(CONTEXT), None);
        h.feed(vec![Event::TurnEnd("done".into())]).await;
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some(h.planned_id.as_str()));
    }

    /// The session a run leaves for a reopened attempt of its task is the one it commits, on the
    /// same condition: the reported id over the handed one, and nothing from a run that produced
    /// no work or lost its session.
    #[tokio::test]
    async fn harness_session_a_run_carries_to_a_reopen_exactly_what_it_commits() {
        let mut h = Harness::new(10).await;
        h.feed(vec![session_started("worked"), text("an answer")])
            .await;
        assert_eq!(h.carried.as_deref(), Some("worked"));

        let mut h = Harness::new(10).await;
        h.feed(vec![Event::TurnEnd("done".into())]).await;
        assert_eq!(h.carried.as_deref(), Some(h.planned_id.as_str()));

        let mut h = Harness::new(10).await;
        h.feed(vec![session_started("named-but-empty")]).await;
        assert_eq!(h.carried, None);

        let map = Arc::new(HarnessSessionMap::new(None, Path::new("/tmp")));
        map.put(CONTEXT, "remembered", HARNESS, "fixture-driver");
        let mut h = Harness::threading(10, map).await;
        h.feed(vec![Event::TurnFailed(TurnFailure {
            kind: FailureKind::HarnessError,
            message: "no conversation found with session id remembered".into(),
        })])
        .await;
        assert_eq!(h.carried, None);
    }

    /// Nothing was established, so nothing is remembered.
    #[tokio::test]
    async fn harness_session_a_failed_run_with_no_session_remembers_nothing() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::TurnFailed(TurnFailure {
            kind: FailureKind::Auth,
            message: "not signed in".into(),
        })])
        .await;
        assert_eq!(h.map.get(CONTEXT), None);
    }

    /// A resumed turn the harness never acknowledged fails as a missing conversation, and leaves
    /// the entry exactly as it was so the next task fails the same way.
    #[tokio::test]
    async fn harness_session_a_resume_the_harness_never_acknowledged_fails_as_gone() {
        let map = Arc::new(HarnessSessionMap::new(None, Path::new("/tmp")));
        map.put(CONTEXT, "remembered", HARNESS, "fixture-driver");
        let mut h = Harness::threading(10, map).await;
        assert_eq!(h.planned_id, "remembered");

        let outcome = h
            .feed(vec![Event::TurnFailed(TurnFailure {
                kind: FailureKind::HarnessError,
                message: "no conversation found with session id remembered".into(),
            })])
            .await;
        let SinkOutcome::Failed(error) = outcome else {
            panic!("a resume the harness could not find fails the turn");
        };
        assert!(
            matches!(error, RuntimeError::HarnessSessionGone { .. }),
            "{error}"
        );
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some("remembered"));
        // The harness still said what happened, in the trace, under its own kind.
        assert_eq!(
            h.of_type("harness_failed").await[0]["kind"],
            "harness-error"
        );
    }

    /// An expired login is not a missing conversation.
    #[tokio::test]
    async fn harness_session_an_auth_failure_on_a_resume_is_still_a_turn_failure() {
        let map = Arc::new(HarnessSessionMap::new(None, Path::new("/tmp")));
        map.put(CONTEXT, "remembered", HARNESS, "fixture-driver");
        let mut h = Harness::threading(10, map).await;

        let outcome = h
            .feed(vec![Event::TurnFailed(TurnFailure {
                kind: FailureKind::Auth,
                message: "not signed in".into(),
            })])
            .await;
        let SinkOutcome::Failed(error) = outcome else {
            panic!("a failed turn fails the run");
        };
        assert!(
            matches!(error, RuntimeError::HarnessTurnFailed { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn harness_session_a_resumed_turn_that_ends_keeps_its_id() {
        let map = Arc::new(HarnessSessionMap::new(None, Path::new("/tmp")));
        map.put(CONTEXT, "remembered", HARNESS, "fixture-driver");
        let mut h = Harness::threading(10, map).await;
        assert_eq!(h.planned_id, "remembered");

        h.feed(vec![
            Event::SessionStarted(SessionInfo {
                id: "remembered".into(),
                auth: SUBSCRIPTION_AUTH.into(),
                model: None,
            }),
            Event::Text("here it is".into()),
            Event::TurnEnd("here it is".into()),
        ])
        .await;
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some("remembered"));
    }
}
