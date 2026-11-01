//! What the runtime does with the typed events a process driver reads out of its harness.
//!
//! The driver turns harness output into [`Event`]s; this module is the other half — it turns
//! those events into everything a turn records: the `inference` and `tool_call` trace events, the
//! `Inference` and `ToolCall` hooks, the OTel spans, the result text, the harness's own
//! diagnostics, and, through [`A2aStream`], the task's A2A frames. The runtime never reads the
//! harness's output itself, so nothing here interprets text; it only counts turns and writes
//! records.
//!
//! The sink also builds the one `Inference` hook event no harness event produces: the
//! announcement of a bridged tool call, dispatched by [`ProcessEventSink::announce_call`] at the
//! call's decision point, before it runs. The turn's own `Inference` event still fires when the
//! turn closes.
//!
//! # Turns
//!
//! One logical turn is one model action, counted as the `transport: http` path counts it. A turn
//! opens at the first `text` or `tool-call` since the run started or since the last `tool-result`,
//! and closes at the next `tool-result` or terminal event. Thinking, the streaming deltas and a
//! tool call still being written (`tool-call-started`, `tool-call-progress`) never open one: a
//! harness that streams its reasoning as its own events would otherwise burn a turn per thought,
//! and `max_turns` would mean something different on each transport. The call's turn opens when
//! its complete `tool-call` arrives, which is also the event a spent budget stops the run at.
//!
//! An A2A **segment** is a separate notion, held by [`A2aStream`]: it reads the turn counter to
//! number itself and never advances it, and a streamed fragment or a started call opens one where
//! it opens no turn.
//!
//! # What the harness reports about the person's subscription
//!
//! `quota` and `suggested-prompt` are recorded and forwarded, never acted on: neither opens a
//! turn or a segment, neither is observable work, and no quota, however exhausted, warns, stops,
//! retries or changes a prompt.
//!
//! # After the terminal event
//!
//! Everything the harness says after its terminal event — the rest of that event's batch, and the
//! lines the runner reads while the harness exits — goes through
//! [`ProcessEventSink::after_terminal`]: `quota`, `suggested-prompt` and `note` are taken as they
//! would be before it, and every other event is one `harness_note` naming it. Nothing there
//! touches the turn counter, the result, usage, spend or the attempt's ending.
//!
//! # Images a bridged tool returned
//!
//! The bridge sends an [`AttachmentReport`] for each call whose tool named an attachment, and the
//! sink queues it until the harness's `tool-result` closes a call with the same bare name and
//! arguments. The `tool_call` record of that call then carries the report's `attachments`, and
//! every verbatim occurrence of a base64 image the bridge sent is replaced in the recorded output
//! by [`SentImage::placeholder`], so no image bytes reach the trace, a hook or a frame. The only
//! text searched is the base64 the bridge itself produced.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Instant,
};

use murmur_artifact::{security_warning_link, W_SEC_031};
use serde_json::{json, Value};

use crate::{
    agent::DriverUsage,
    credential_gateway::GatewayTable,
    errors::RuntimeError,
    hooks::{HookArtifact, HookEvent, HookRuntime},
    otel::OtelEmitter,
    process_driver::{Event, FailureKind, QuotaWindow, Usage},
    spend::{SpendMeter, SpendRefusal},
    streaming::StreamArtifact,
    trace::{
        FailedInference, TraceWriter, INFERENCE_ERROR_HARNESS_AUTH, INFERENCE_ERROR_HARNESS_ERROR,
        INFERENCE_ERROR_HARNESS_OTHER, INFERENCE_ERROR_HARNESS_QUOTA,
    },
};

use super::claude_bridge::{AttachmentReport, SentImage};
use super::process::RunSession;
use super::process_a2a::A2aStream;
use crate::workdir_fs::WorkdirRoot;

/// The billing mode the process transport exists for. Compared as a plain string: what any other
/// value means is the harness's business, and the runtime only reports that it is not this one.
const SUBSCRIPTION_AUTH: &str = "subscription";

/// The longest window name a `quota` report may carry.
const QUOTA_NAME_MAX_CHARS: usize = 64;

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

/// How many attachment reports no result has claimed yet the sink holds; past this the oldest is
/// dropped.
const MAX_UNCLAIMED_REPORTS: usize = 64;

/// An attachment report waiting for the result of the call it belongs to, keyed as that call's
/// [`PendingToolCall`] is: by bare name and parsed arguments.
struct QueuedReport {
    name: String,
    input: Value,
    report: AttachmentReport,
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
    /// The session's records directory, where `out/result.txt` is written and every hook fault
    /// this run raises is logged.
    records_root: WorkdirRoot,
    /// This attempt's remaining turn budget, already net of the turns earlier attempts spent.
    /// `None` refuses no turn.
    max_turns: Option<u32>,
    /// The task-wide ceiling, `inference.max_turns` as declared, which a run that opens one turn
    /// too many names.
    task_max_turns: Option<u32>,
    /// How many turns have opened in this run.
    turns: u32,
    /// The task-wide number this run's first turn takes: the agent-loop turns earlier attempts of
    /// the task recorded. Every turn this run records is numbered from it.
    first_turn: u32,
    open: Option<OpenTurn>,
    pending: HashMap<String, PendingToolCall>,
    /// Attachment reports no result has claimed yet, oldest first, at most
    /// [`MAX_UNCLAIMED_REPORTS`].
    reports: VecDeque<QueuedReport>,
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
    /// The A2A half of the same events. Borrowed rather than owned, because the attempt reads its
    /// ending from it after this sink is done with the run.
    a2a: &'a mut A2aStream,
    /// The session's gateways, whose credentials are redacted out of a failed call's `error`.
    gateways: GatewayTable,
}

/// The `harness_failed` `max-turns` message: the turn the harness opened, by the task-wide number
/// `mur trace show` prints, and the ceiling it is past — with the turns earlier attempts of the
/// task already used, when there were any.
fn turn_budget_exceeded_message(turn: u32, ceiling: u32, used_before: u32) -> String {
    let earlier = match used_before {
        0 => String::new(),
        1 => "; earlier attempts of this task used 1 of them".to_string(),
        n => format!("; earlier attempts of this task used {n} of them"),
    };
    format!(
        "the harness opened turn {turn}, past the {ceiling} turns inference.max_turns allows \
         this task{earlier}"
    )
}

impl<'a> ProcessEventSink<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        records_root: &WorkdirRoot,
        max_turns: Option<u32>,
        task_max_turns: Option<u32>,
        first_turn: u32,
        session: RunSession,
        spend: Arc<SpendMeter>,
        a2a: &'a mut A2aStream,
        gateways: GatewayTable,
    ) -> Self {
        Self {
            records_root: records_root.clone(),
            max_turns,
            task_max_turns,
            turns: 0,
            first_turn,
            open: None,
            pending: HashMap::new(),
            reports: VecDeque::new(),
            finished: false,
            session,
            reported_session: None,
            usage: UsageCounts::default(),
            attributed: UsageCounts::default(),
            spend,
            produced: false,
            session_gone: false,
            a2a,
            gateways,
        }
    }

    /// The session's records directory, which this run writes `out/result.txt` into.
    pub(super) fn records_root(&self) -> &WorkdirRoot {
        &self.records_root
    }

    /// Hold `report` until a result closes the call it belongs to. Arguments that are not JSON key
    /// as `null`, as the decision point reads them. Past [`MAX_UNCLAIMED_REPORTS`] the oldest
    /// report is dropped.
    pub(super) fn queue_report(&mut self, report: AttachmentReport) {
        if self.reports.len() >= MAX_UNCLAIMED_REPORTS {
            self.reports.pop_front();
        }
        self.reports.push_back(QueuedReport {
            name: report.tool_name.clone(),
            input: serde_json::from_str(&report.input_json).unwrap_or(Value::Null),
            report,
        });
    }

    /// The queued report for a call of `name` with `input` whose result is `output`, removed from
    /// the queue: the oldest one with an image `output` repeats, otherwise the oldest one.
    ///
    /// Reports queue in the order answers were written, which is not the order a harness reports
    /// results in when two calls share a name and arguments. Taking the oldest alone would pair
    /// such a result with the other call's report and leave its own image's bytes unreplaced.
    fn claim_report(
        &mut self,
        name: &str,
        input: &Value,
        output: &str,
    ) -> Option<AttachmentReport> {
        let same_call = |queued: &QueuedReport| queued.name == name && queued.input == *input;
        let repeated = |queued: &QueuedReport| {
            queued
                .report
                .images
                .iter()
                .any(|image| !image.base64.is_empty() && output.contains(&image.base64))
        };
        let index = self
            .reports
            .iter()
            .position(|queued| same_call(queued) && repeated(queued))
            .or_else(|| self.reports.iter().position(same_call))?;
        self.reports.remove(index).map(|queued| queued.report)
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

    /// The turn a record written now belongs to: the open turn, or the last one that closed —
    /// `first_turn` before any has opened.
    pub(super) fn current_turn(&self) -> u32 {
        match self.open.as_ref() {
            Some(open) => open.index,
            None => self.first_turn + self.turns.saturating_sub(1),
        }
    }

    /// The turn a bridged call belongs to: the open turn, or the turn the harness's report of the
    /// call will open.
    ///
    /// The bridge receives a call before the harness's `tool-call` event for it has been read, so
    /// with no turn open the call's turn is the next one. Best-effort when a harness message holds
    /// several calls: a call answered after an earlier one's `tool-result` closed the turn is
    /// numbered with the turn after it.
    pub(super) fn call_turn(&self) -> u32 {
        match self.open.as_ref() {
            Some(open) => open.index,
            None => self.first_turn + self.turns,
        }
    }

    /// Announce one bridged call to every `on-inference` hook, before the call runs.
    ///
    /// Called from the call's decision point with the bare tool name the bridge received and the
    /// argument object dispatch is about to be handed. The announcement is not a turn: it writes
    /// no trace record, opens no turn, and leaves the session's running totals where they are
    /// (see [`HookRuntime::announce`]).
    pub(super) async fn announce_call(&self, hooks: &mut HookRuntime, name: &str, input: &Value) {
        hooks
            .announce(
                &self.records_root,
                call_announcement(self.call_turn(), name, input),
            )
            .await;
    }

    /// Feed one `parse` batch through. Events after a terminal one, in the same batch or a later
    /// one, go through [`Self::after_terminal`]: the run is over and the harness is already being
    /// shut down.
    pub(super) async fn consume(
        &mut self,
        events: Vec<Event>,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) -> SinkOutcome {
        let mut outcome = SinkOutcome::Continue;
        for event in events {
            if self.finished {
                self.after_terminal(event, trace).await;
                continue;
            }
            match self.handle(event, hooks, trace, otel).await {
                SinkOutcome::Continue => {}
                terminal => {
                    self.finished = true;
                    outcome = terminal;
                }
            }
        }
        outcome
    }

    /// One event the harness reported after the turn's terminal event.
    ///
    /// `quota`, `suggested-prompt` and `note` are taken as they would be before it. Every other
    /// event is written as one `harness_note` naming its WIT case, without its payload, and
    /// ignored: it never changes the turn's outcome, its result, its usage or its spend.
    pub(super) async fn after_terminal(&mut self, event: Event, trace: &mut TraceWriter) {
        match event {
            Event::Quota(windows) => self.record_quota(windows, trace).await,
            Event::SuggestedPrompt(text) => self.record_suggestion(&text, trace).await,
            Event::Note(text) => {
                let _ = trace.write_harness_note(&text).await;
            }
            other => {
                let _ = trace
                    .write_harness_note(&format!(
                        "{} after the turn's terminal event, ignored",
                        event_case_name(&other)
                    ))
                    .await;
            }
        }
    }

    /// One `quota` report: each window refused by [`validate_quota`] is named in one
    /// `harness_note`, and the rest, when any stand, are recorded and forwarded as the driver
    /// sent them.
    async fn record_quota(&mut self, windows: Vec<QuotaWindow>, trace: &mut TraceWriter) {
        let (valid, refused) = validate_quota(windows);
        if let Some(note) = refused {
            let _ = trace.write_harness_note(&note).await;
        }
        if valid.is_empty() {
            return;
        }
        let _ = trace.write_harness_quota(&valid).await;
        self.a2a.quota(&valid);
    }

    /// One suggested prompt. An empty one says nothing and records nothing.
    async fn record_suggestion(&mut self, text: &str, trace: &mut TraceWriter) {
        if text.is_empty() {
            return;
        }
        let _ = trace.write_harness_suggestion(text).await;
        self.a2a.suggested_prompt(text);
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
        // A report no result claimed belongs to a call the harness never reported back.
        self.reports.clear();
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
                // The call's own frame is the `artifact` its result writes. What this event writes
                // is the size a started call was still holding, so the client's last
                // `tool-call-progress` is the complete input's.
                self.a2a.open_segment(self.segment_turn()).await;
                let started = self.a2a.tool_call_complete(&call.id, Instant::now()).await;
                if let Some(started) = started.filter(|started| *started != call.name) {
                    let _ = trace
                        .write_harness_note(&format!(
                            "the harness started tool call id {} as '{started}' and completed it \
                             as '{}'",
                            call.id, call.name
                        ))
                        .await;
                }
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
                // A turn that asked for a tool is not the attempt's last inference turn, and its
                // `on-inference` artifacts are dropped, as the http path drops that turn's.
                if let Err(reached) = self.close_turn(hooks, trace, otel).await {
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
                let (output, attachments) =
                    match self.claim_report(&call.name, &call.input, &result.output) {
                        Some(report) => (
                            without_sent_images(result.output, &report.images),
                            report.trace,
                        ),
                        None => (result.output, Vec::new()),
                    };
                let output_bytes = output.len() as u64;
                let result_id = result.id.clone();
                // Measured by the runtime rather than taken from the harness: the bridge executes
                // the tool, and this is the only clock that saw both ends of the call.
                let duration_ms = call
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                let _ = trace
                    .write_tool_call_with_attachments(
                        call.turn,
                        call.name.clone(),
                        Some(result.id),
                        call.input.clone(),
                        call.input_bytes,
                        &output,
                        output_bytes,
                        duration_ms,
                        status.to_string(),
                        None,
                        None,
                        None,
                        attachments,
                    )
                    .await;
                hooks
                    .emit(
                        &self.records_root,
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
                // and the event carries no truncation flag, so those three keys say so. Its
                // `attachments` stay empty: nothing is stored on this transport, so there is no
                // address an image could be fetched from.
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
            // A tool call the model is still writing. Like a fragment it opens no turn: the call's
            // turn opens, and is counted against the budget, when the complete `tool-call`
            // arrives. Nothing is traced: the `tool_call` record at its result is the record of
            // the call.
            Event::ToolCallStarted(start) => {
                self.a2a
                    .tool_call_started(self.segment_turn(), &start.id, &start.name)
                    .await;
                SinkOutcome::Continue
            }
            Event::ToolCallProgress(progress) => {
                self.a2a
                    .tool_call_progress(&progress.id, progress.input_bytes, Instant::now())
                    .await;
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
            Event::Quota(windows) => {
                self.record_quota(windows, trace).await;
                SinkOutcome::Continue
            }
            Event::SuggestedPrompt(text) => {
                self.record_suggestion(&text, trace).await;
                SinkOutcome::Continue
            }
            Event::TurnEnd(result) => {
                // A ceiling the last turn reached outranks the answer: the run stopped at the
                // operator's limit, and `out/result.txt` says so rather than holding an answer
                // the next turn would have gone past the ceiling to improve on.
                let hook_artifacts = match self.close_turn(hooks, trace, otel).await {
                    Ok(hook_artifacts) => hook_artifacts,
                    Err(reached) => return reached,
                };
                match super::record_result(hooks, &self.records_root, &result) {
                    Ok(()) => {
                        self.a2a.turn_end(&result, hook_artifacts).await;
                        SinkOutcome::Ended
                    }
                    Err(message) => SinkOutcome::Failed(RuntimeError::AgentLoopFailed(message)),
                }
            }
            Event::TurnFailed(failure) => {
                // The failure is the run's own account of itself and outranks a ceiling the same
                // turn reached: the harness said why it stopped, and that is what is reported. A
                // failed attempt forwards no hook artifact. A budget or a cancel is no failed
                // model call, so its open turn closes as an ordinary one.
                let _ = match failed_call_code(failure.kind) {
                    Some(code) => {
                        self.record_failed_call(hooks, trace, otel, code, &failure.message)
                            .await
                    }
                    None => self.close_turn(hooks, trace, otel).await,
                };
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
            None => self.first_turn + self.turns + 1,
        }
    }

    /// Open a turn if none is open. Returns `Some` when doing so would exceed the attempt's
    /// budget, in which case no turn is opened and the run is over.
    async fn open_turn(&mut self, trace: &mut TraceWriter) -> Option<SinkOutcome> {
        if self.open.is_some() {
            return None;
        }
        self.turns += 1;
        if let Some(max_turns) = self.max_turns.filter(|max_turns| self.turns > *max_turns) {
            let ceiling = self.task_max_turns.unwrap_or(max_turns);
            let message = turn_budget_exceeded_message(
                self.first_turn + self.turns - 1,
                ceiling,
                ceiling.saturating_sub(max_turns),
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
            index: self.first_turn + self.turns - 1,
            text: None,
            first_tool: None,
        });
        None
    }

    /// Write the open turn's `inference` record, if one is open, charge what it cost, and hand
    /// back the artifacts its `on-inference` hooks returned — none when no turn was open — or the
    /// run's end when that took the session to a spend ceiling.
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
    ) -> Result<Vec<HookArtifact>, SinkOutcome> {
        self.close_turn_as(hooks, trace, otel, None).await
    }

    /// Record a model call the harness failed, with `error_code` from the `INFERENCE_ERROR_*`
    /// vocabulary and `message` with the session's credentials redacted.
    ///
    /// A turn that is open closes exactly as [`Self::close_turn`] closes it — counted, with the
    /// harness's own counts, its OTel span, its `on-inference` hook and its spend charge — except
    /// that its record and its `decision` say it failed. With no turn open the record is written
    /// at the turn that would open next and counts nothing.
    pub(super) async fn record_failed_call(
        &mut self,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
        error_code: &'static str,
        message: &str,
    ) -> Result<Vec<HookArtifact>, SinkOutcome> {
        let error = self.gateways.redact(message);
        if self.open.is_none() {
            let _ = trace
                .write_failed_inference(FailedInference {
                    turn: self.first_turn + self.turns,
                    error_code,
                    error: &error,
                    provider_status: None,
                    usage: None,
                    origin: None,
                    choice: None,
                    message_ids: Vec::new(),
                    tool_name: None,
                    spent: None,
                    settings: None,
                    retry: crate::inference_retry::RetryNote::default(),
                    // The process transport sends nothing through a runtime gateway.
                    gateway: None,
                })
                .await;
            return Ok(Vec::new());
        }
        self.close_turn_as(hooks, trace, otel, Some((error_code, &error)))
            .await
    }

    /// [`Self::close_turn`], recording the turn as a failed call when `failure` names one as
    /// `(error_code, error)`.
    async fn close_turn_as(
        &mut self,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
        failure: Option<(&'static str, &str)>,
    ) -> Result<Vec<HookArtifact>, SinkOutcome> {
        let Some(open) = self.open.take() else {
            return Ok(Vec::new());
        };
        let decision = if failure.is_some() {
            "error"
        } else if open.first_tool.is_some() {
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
        let _ = match failure {
            Some((error_code, error)) => {
                trace
                    .write_failed_inference(FailedInference {
                        turn: open.index,
                        error_code,
                        error,
                        // A harness reports no HTTP status.
                        provider_status: None,
                        usage: reported.as_ref(),
                        origin: None,
                        choice: None,
                        message_ids: Vec::new(),
                        tool_name: open.first_tool.clone(),
                        spent: Some((spent.input, spent.output)),
                        settings: None,
                        retry: crate::inference_retry::RetryNote::default(),
                        // The process transport sends nothing through a runtime gateway.
                        gateway: None,
                    })
                    .await
            }
            None => {
                trace
                    .write_inference(
                        open.index,
                        spent.input,
                        spent.output,
                        decision.to_string(),
                        // The runtime reads no driver response of its own on this transport, so
                        // there is no provider stop reason and no request payload to hash.
                        None,
                        open.first_tool.clone(),
                        None,
                        reported.as_ref(),
                        Vec::new(),
                        None,
                        None,
                    )
                    .await
            }
        };
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
        // The turn's own `on-inference`, carrying its counts. Each call the turn made was
        // announced on its own at the decision point, before it ran (`announce_call`).
        //
        // No hook inference flush after this emit or the `on-tool-call` one: a hook's
        // `run-inference` is answered with `InferenceUnavailable::ProcessTransport` on this
        // transport, so it buffers no `HookInferenceRecord` or spend refusal. A change that lets
        // the call through must flush at both, ahead of the turn's own record, as http does.
        let hook_artifacts = hooks
            .emit(
                &self.records_root,
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
        let Some(refusal) = self.spend.ceiling_crossed() else {
            return Ok(hook_artifacts);
        };
        let _ = trace
            .write_spend_ceiling_reached(open.index, &refusal, None)
            .await;
        self.a2a.mark_spend_refused(&refusal);
        Err(SinkOutcome::SpendCeilingReached(refusal))
    }
}

/// `output` with every verbatim occurrence of each image's base64 replaced by its
/// [`SentImage::placeholder`]. A driver may hand a harness's tool result back with the bridge's
/// image items inside it, and the bytes are the runtime's own to keep out of its records.
fn without_sent_images(mut output: String, images: &[SentImage]) -> String {
    for image in images {
        if !image.base64.is_empty() && output.contains(&image.base64) {
            output = output.replace(&image.base64, &image.placeholder());
        }
    }
    output
}

/// The `on-inference` event announcing one bridged call, in the shape `transport: http` hands a
/// hook a tool-calling turn: zero counts, a `tool_call` decision, the call's bare name, and an
/// `output` holding one `tool_call` content block. The block carries no `id`, because a bridged
/// call has no provider tool-use id, and `input` is the argument object itself. `prompt` and
/// `tools` stay absent: the harness builds the request, so the runtime holds neither.
fn call_announcement(turn: u32, name: &str, input: &Value) -> HookEvent {
    HookEvent::Inference {
        turn,
        input_tokens: 0,
        output_tokens: 0,
        decision: "tool_call".to_string(),
        tool_name: Some(name.to_string()),
        prompt: None,
        output: Some(json!([{ "type": "tool_call", "name": name, "input": input }]).to_string()),
        tools: None,
    }
}

/// Whether an event is work the harness actually produced, which is the whole of what makes a
/// run's session id worth keeping.
///
/// Eight events say yes: a complete or streamed piece of text, a complete or streamed thought, a
/// tool call complete or still being written, and a turn that ended. `turn-end` counts whatever
/// its result text says — a turn that ran to completion is a conversation the harness holds,
/// however little it had to say.
///
/// The rest say no, and `session-started` is why this predicate exists: a harness prints the name
/// of its session before the model has produced anything and before it has written a transcript,
/// so a run that got no further leaves the harness with nothing to answer to under that name. A
/// retry, a note, a tool result and a failed turn each report on work rather than being it, and a
/// quota report and a suggested prompt are what the harness says about the person, not work.
fn produces_observable_work(event: &Event) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::TextDelta(_)
            | Event::Thinking(_)
            | Event::ThinkingDelta(_)
            | Event::ToolCall(_)
            | Event::ToolCallStarted(_)
            | Event::ToolCallProgress(_)
            | Event::TurnEnd(_)
    )
}

/// Splits one `quota` report into the windows that stand and the `harness_note` text naming each
/// refused window and why, `None` when none was refused.
///
/// A window stands when its `name` is 1 to [`QUOTA_NAME_MAX_CHARS`] characters of `a`-`z`, `0`-`9`
/// and `-`, starting with a letter or digit, and its `used`, when present, is a finite number from
/// 0 to 1. Of several standing windows with one name, the last stands, in the place the first
/// held. Nothing else is checked: what the members mean is the harness's to say.
fn validate_quota(windows: Vec<QuotaWindow>) -> (Vec<QuotaWindow>, Option<String>) {
    let mut valid: Vec<QuotaWindow> = Vec::new();
    let mut refused = Vec::new();
    for window in windows {
        let shown: String = window.name.chars().take(QUOTA_NAME_MAX_CHARS).collect();
        if !valid_quota_name(&window.name) {
            refused.push(format!(
                "{shown:?}: the name is not 1 to {QUOTA_NAME_MAX_CHARS} characters of a-z, 0-9 \
                 and '-' starting with a letter or digit"
            ));
            continue;
        }
        if let Some(used) = window.used.filter(|used| !(0.0..=1.0).contains(used)) {
            refused.push(format!(
                "{shown:?}: used {used} is not a fraction from 0 to 1"
            ));
            continue;
        }
        match valid.iter_mut().find(|held| held.name == window.name) {
            Some(held) => *held = window,
            None => valid.push(window),
        }
    }
    let note = (!refused.is_empty()).then(|| {
        format!(
            "the harness's quota report named windows the runtime refused, and they were dropped: \
             {}",
            refused.join("; ")
        )
    });
    (valid, note)
}

/// Whether `name` is a window name a `quota` report may carry.
fn valid_quota_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.chars().count() <= QUOTA_NAME_MAX_CHARS
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The WIT case name of `event`, which is all a note about an ignored event says of it.
fn event_case_name(event: &Event) -> &'static str {
    match event {
        Event::SessionStarted(_) => "session-started",
        Event::TextDelta(_) => "text-delta",
        Event::Text(_) => "text",
        Event::ThinkingDelta(_) => "thinking-delta",
        Event::Thinking(_) => "thinking",
        Event::ToolCall(_) => "tool-call",
        Event::ToolResult(_) => "tool-result",
        Event::Retry(_) => "retry",
        Event::Usage(_) => "usage",
        Event::TurnEnd(_) => "turn-end",
        Event::TurnFailed(_) => "turn-failed",
        Event::Note(_) => "note",
        Event::ToolCallStarted(_) => "tool-call-started",
        Event::ToolCallProgress(_) => "tool-call-progress",
        Event::Quota(_) => "quota",
        Event::SuggestedPrompt(_) => "suggested-prompt",
    }
}

/// The `inference.error_code` a harness failure of `kind` records, or `None` for a kind that is
/// a budget or a cancel rather than a failed model call.
fn failed_call_code(kind: FailureKind) -> Option<&'static str> {
    match kind {
        FailureKind::Auth => Some(INFERENCE_ERROR_HARNESS_AUTH),
        FailureKind::Quota => Some(INFERENCE_ERROR_HARNESS_QUOTA),
        FailureKind::HarnessError => Some(INFERENCE_ERROR_HARNESS_ERROR),
        FailureKind::Other => Some(INFERENCE_ERROR_HARNESS_OTHER),
        FailureKind::MaxTurns | FailureKind::Canceled => None,
    }
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
    use std::path::{Path, PathBuf};
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    use super::*;
    use crate::agent::process::{plan_session, HarnessSessionPolicy, SessionPlan};
    use crate::harness_session::HarnessSessionMap;
    use crate::process_driver::{
        QuotaState, RetryInfo, SessionInfo, ToolCallInfo, ToolCallProgress, ToolCallStart,
        ToolResultInfo, TurnFailure,
    };
    use crate::streaming::{buffered, event_kind, frame_of, SseEventBuffer};
    use serde_json::Value as Json;

    /// The context every sink below runs a task under.
    const CONTEXT: &str = "ctx_test";

    /// The harness name every sink below records with the session it remembers.
    const HARNESS: &str = "fixture-harness";

    /// The label an event reads as: its kind, and for the two kinds whose shape depends on a
    /// key, the key. The same labelling the process/http parity tests compare sequences with.
    fn frame_kind(event: &Json) -> String {
        match event_kind(event).as_str() {
            "status-update" => {
                let status = &event["statusUpdate"]["status"];
                let state = status["state"].as_str().unwrap_or_default();
                let state = crate::a2a::murmur_state_name(state).unwrap_or(state);
                match state {
                    "working" => format!(
                        "status:working:{}",
                        status["message"]["parts"][0]["text"]
                            .as_str()
                            .unwrap_or_default()
                    ),
                    state => format!("status:{state}"),
                }
            }
            "text" => match frame_of(event)["final"] == Json::Bool(true) {
                true => "text:final".to_string(),
                false => "text:chunk".to_string(),
            },
            other => other.to_string(),
        }
    }

    /// What every event of `kind` says, in order: a murmur frame's payload, or a
    /// `statusUpdate`'s body for `"status"`.
    fn data_of(events: &[Json], kind: &str) -> Vec<Json> {
        events
            .iter()
            .filter_map(|event| match (kind, event_kind(event).as_str()) {
                ("status", "status-update") => Some(event["statusUpdate"].clone()),
                (wanted, found) if wanted == found => Some(frame_of(event)),
                _ => None,
            })
            .collect()
    }

    /// The trace every sink below writes, at `workdir`, capturing `capture`.
    async fn open_trace(workdir: &Path, capture: murmur_artifact::TraceCapture) -> TraceWriter {
        TraceWriter::open(
            &crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            "ses_test".to_string(),
            "cap".to_string(),
            "0.1.0".to_string(),
            "test-model".to_string(),
            Vec::new(),
            crate::containment::report_for(
                &crate::CapabilityPolicy::default(),
                murmur_artifact::ContainmentClass::Advisory,
                crate::sandbox::EnforcementTier::EnvironmentOnly,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    crate::sandbox::EnforcementTier::EnvironmentOnly,
                    murmur_artifact::ContainmentClass::Advisory,
                )),
                crate::cgroup::IoMaxReport::default(),
            ),
            capture,
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// A sink's sinks and the workdir they write into. Held together so a test can drop the
    /// tempdir only after reading `trace.jsonl` back, and so one A2A stream spans every batch a
    /// test feeds.
    struct Harness {
        _dir: tempfile::TempDir,
        workdir: PathBuf,
        max_turns: Option<u32>,
        /// The task-wide ceiling the sink names; [`Self::max_turns`] unless a test continues a
        /// task.
        task_max_turns: Option<u32>,
        /// The task-wide number the sink's first turn takes; 0 unless a test continues a task.
        first_turn: u32,
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
        /// The sink's `current_turn` at the end of the last batch.
        turn: u32,
    }

    impl Harness {
        async fn new(max_turns: u32) -> Self {
            Self::threading(
                max_turns,
                Arc::new(HarnessSessionMap::new(
                    None,
                    &crate::workdir_fs::WorkdirRoot::open(Path::new("/tmp")).unwrap(),
                )),
            )
            .await
        }

        /// The same harness, running a continued attempt of a task that already recorded
        /// `first_turn` turns.
        fn continuing_from(mut self, first_turn: u32) -> Self {
            self.first_turn = first_turn;
            self
        }

        /// The same harness, under a task-wide `inference.max_turns` of `ceiling`.
        fn under_ceiling(mut self, ceiling: u32) -> Self {
            self.task_max_turns = Some(ceiling);
            self
        }

        /// The same harness, writing `trace.capture: content`.
        async fn capturing_content(mut self) -> Self {
            self.trace = open_trace(&self.workdir, murmur_artifact::TraceCapture::Content).await;
            self
        }

        /// The same harness, with no `inference.max_turns`.
        fn uncapped(mut self) -> Self {
            self.max_turns = None;
            self.task_max_turns = None;
            self
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
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
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
                Err(crate::inference_import::InferenceUnavailable::NotConfigured),
                None,
            )
            .await
            .unwrap();
            let trace = open_trace(&workdir, murmur_artifact::TraceCapture::Meta).await;
            let otel = OtelEmitter::new(
                None,
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
                "cap".to_string(),
                "0.1.0".to_string(),
            );
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
                max_turns: Some(max_turns),
                task_max_turns: Some(max_turns),
                first_turn: 0,
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
                turn: 0,
            }
        }

        /// The sink is built per batch because it borrows the stream; its turn state is a
        /// test-local concern and every test here feeds exactly one batch.
        async fn feed(&mut self, events: Vec<Event>) -> SinkOutcome {
            self.feed_with_reports(Vec::new(), events).await
        }

        /// [`Self::feed`], with `reports` queued as the run loop's report arm queues them, ahead
        /// of the batch.
        async fn feed_with_reports(
            &mut self,
            reports: Vec<AttachmentReport>,
            events: Vec<Event>,
        ) -> SinkOutcome {
            let session = RunSession::new(&self.plan, &self.policy, HARNESS, "fixture-driver");
            let mut sink = ProcessEventSink::new(
                &crate::workdir_fs::WorkdirRoot::open(&self.workdir).unwrap(),
                self.max_turns,
                self.task_max_turns,
                self.first_turn,
                session,
                Arc::clone(&self.spend),
                &mut self.a2a,
                GatewayTable::default(),
            );
            for report in reports {
                sink.queue_report(report);
            }
            let outcome = sink
                .consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await;
            self.turn = sink.current_turn();
            self.carried = sink.finish(&mut self.trace).await;
            outcome
        }

        /// [`Self::feed`], then `late` as events read while the harness exits after its terminal
        /// event, which `process.rs` hands to [`ProcessEventSink::after_terminal`].
        async fn feed_then_late(&mut self, events: Vec<Event>, late: Vec<Event>) -> SinkOutcome {
            let session = RunSession::new(&self.plan, &self.policy, HARNESS, "fixture-driver");
            let mut sink = ProcessEventSink::new(
                &crate::workdir_fs::WorkdirRoot::open(&self.workdir).unwrap(),
                self.max_turns,
                self.task_max_turns,
                self.first_turn,
                session,
                Arc::clone(&self.spend),
                &mut self.a2a,
                GatewayTable::default(),
            );
            let outcome = sink
                .consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await;
            for event in late {
                sink.after_terminal(event, &mut self.trace).await;
            }
            self.turn = sink.current_turn();
            self.carried = sink.finish(&mut self.trace).await;
            outcome
        }

        /// [`Self::feed`], then the harness goes silent past the inactivity timeout before the
        /// run reads another line: the sink records the failed call as `process.rs`'s
        /// `RunEnd::Inactive` arm does.
        async fn feed_then_go_quiet(&mut self, events: Vec<Event>) {
            let session = RunSession::new(&self.plan, &self.policy, HARNESS, "fixture-driver");
            let mut sink = ProcessEventSink::new(
                &crate::workdir_fs::WorkdirRoot::open(&self.workdir).unwrap(),
                self.max_turns,
                self.task_max_turns,
                self.first_turn,
                session,
                Arc::clone(&self.spend),
                &mut self.a2a,
                GatewayTable::default(),
            );
            sink.consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await;
            let _ = sink
                .record_failed_call(
                    &mut self.hooks,
                    &mut self.trace,
                    &mut self.otel,
                    crate::trace::INFERENCE_ERROR_HARNESS_INACTIVE,
                    "the harness went quiet",
                )
                .await;
            self.turn = sink.current_turn();
            self.carried = sink.finish(&mut self.trace).await;
        }

        /// Each frame the A2A stream wrote, labelled by kind, in order.
        fn frame_kinds(&self) -> Vec<String> {
            self.frames().iter().map(frame_kind).collect()
        }

        /// Each event's `StreamResponse`, as an activated connection reads it, in order.
        fn frames(&self) -> Vec<Json> {
            buffered(&self.frames)
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

    fn tool_call_started(id: &str, name: &str) -> Event {
        Event::ToolCallStarted(ToolCallStart {
            id: id.to_string(),
            name: name.to_string(),
        })
    }

    fn tool_call_progress(id: &str, input_bytes: u64) -> Event {
        Event::ToolCallProgress(ToolCallProgress {
            id: id.to_string(),
            input_bytes,
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

    /// A bridged call is announced in the shape `transport: http` hands a hook a tool-calling
    /// turn: zero counts, a `tool_call` decision, the bare name, and one `tool_call` block in
    /// `output` whose `input` is the argument object and which has no `id`.
    #[test]
    fn a_call_announcement_carries_one_tool_call_block() {
        let input = serde_json::json!({"path": "notes.txt", "content": "hello"});
        let HookEvent::Inference {
            turn,
            input_tokens,
            output_tokens,
            decision,
            tool_name,
            prompt,
            output,
            tools,
        } = call_announcement(4, "tool-write", &input)
        else {
            panic!("an announcement is an Inference event");
        };
        assert_eq!(turn, 4);
        assert_eq!((input_tokens, output_tokens), (0, 0));
        assert_eq!(decision, "tool_call");
        assert_eq!(tool_name.as_deref(), Some("tool-write"));
        assert_eq!(prompt, None);
        assert_eq!(tools, None);
        let blocks: Json = serde_json::from_str(&output.expect("output carries the call")).unwrap();
        let blocks = blocks
            .as_array()
            .expect("output is an array of content blocks");
        assert_eq!(blocks.len(), 1, "{blocks:#?}");
        let block = blocks[0].as_object().unwrap();
        assert_eq!(block["type"], "tool_call");
        assert_eq!(block["name"], "tool-write");
        assert_eq!(block["input"]["path"], "notes.txt");
        assert_eq!(
            block["input"], input,
            "input is the argument object, not a string"
        );
        assert!(
            !block.contains_key("id"),
            "a bridged call has no tool-use id"
        );
    }

    /// `call_turn` is the open turn while one is open, and otherwise the turn the harness's
    /// report of the call will open — one past the last closed turn, unlike `current_turn`.
    #[tokio::test]
    async fn a_call_announced_with_no_turn_open_takes_the_next_turn() {
        let mut h = Harness::new(10).await.continuing_from(3);
        let session = RunSession::new(&h.plan, &h.policy, HARNESS, "fixture-driver");
        let mut sink = ProcessEventSink::new(
            &crate::workdir_fs::WorkdirRoot::open(&h.workdir).unwrap(),
            h.max_turns,
            h.task_max_turns,
            h.first_turn,
            session,
            Arc::clone(&h.spend),
            &mut h.a2a,
            GatewayTable::default(),
        );
        assert_eq!(
            sink.call_turn(),
            3,
            "no turn opened yet: the run's first turn"
        );

        sink.consume(
            vec![tool_call("c1", "echo-tool")],
            &mut h.hooks,
            &mut h.trace,
            &mut h.otel,
        )
        .await;
        assert_eq!(sink.call_turn(), 3, "the open turn");
        assert_eq!(sink.call_turn(), sink.current_turn());

        sink.consume(
            vec![tool_result("c1", false)],
            &mut h.hooks,
            &mut h.trace,
            &mut h.otel,
        )
        .await;
        assert_eq!(sink.current_turn(), 3, "the turn that closed");
        assert_eq!(sink.call_turn(), 4, "the turn the next call will open");
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

    /// A continued attempt numbers its turns on from the task's earlier ones, on the trace and on
    /// the A2A stream alike, while its budget still counts only its own turns.
    #[tokio::test]
    async fn a_continued_attempt_numbers_its_turns_from_the_task_s_first_turn() {
        let mut h = Harness::new(2).await.continuing_from(3);
        h.feed(Vec::new()).await;
        assert_eq!(
            h.turn, 3,
            "before any turn opens, the current turn is the first one"
        );

        let mut h = Harness::new(2).await.continuing_from(3);
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            text("all done"),
            Event::TurnEnd("all done".into()),
        ])
        .await;
        let turns: Vec<Json> = h
            .of_type("inference")
            .await
            .into_iter()
            .map(|record| record["turn"].clone())
            .collect();
        assert_eq!(turns, [3, 4]);
        assert_eq!(h.of_type("tool_call").await[0]["turn"], 3);
        assert_eq!(h.turn, 4);
        let statuses: Vec<String> = h
            .frame_kinds()
            .into_iter()
            .filter(|kind| kind.starts_with("status:working:inference turn"))
            .collect();
        assert_eq!(
            statuses,
            [
                "status:working:inference turn 4",
                "status:working:inference turn 5"
            ]
        );

        let mut h = Harness::new(2).await.continuing_from(3);
        let outcome = h
            .feed(vec![
                tool_call("c1", "t"),
                tool_result("c1", false),
                tool_call("c2", "t"),
                tool_result("c2", false),
                text("a third turn"),
            ])
            .await;
        assert!(
            matches!(outcome, SinkOutcome::TurnBudgetExceeded(_)),
            "the attempt's own budget of two turns still bounds it"
        );
        assert_eq!(h.of_type("inference").await.len(), 2);
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

    /// With no `inference.max_turns`, the sink opens every turn the harness reports.
    #[tokio::test]
    async fn no_turn_cap_refuses_no_turn() {
        let mut h = Harness::new(1).await.uncapped();
        let mut events = Vec::new();
        for call in 0..12 {
            let id = format!("c{call}");
            events.push(tool_call(&id, "t"));
            events.push(tool_result(&id, false));
        }
        events.push(text("the last turn"));
        let outcome = h.feed(events).await;
        assert!(
            !matches!(outcome, SinkOutcome::TurnBudgetExceeded(_)),
            "an uncapped run refuses no turn"
        );
        // Each turn is written when the next one opens; the thirteenth is still open.
        assert_eq!(h.of_type("inference").await.len(), 12);
        assert_eq!(h.turn, 12);
        assert!(h.of_type("harness_failed").await.is_empty());
    }

    /// A continued attempt that opens a turn past its budget names that turn as `mur trace show`
    /// numbers it across the task, the task-wide ceiling, and the turns earlier attempts used.
    #[tokio::test]
    async fn a_turn_past_the_budget_names_the_task_wide_turn_and_the_ceiling() {
        let mut h = Harness::new(2).await.continuing_from(3).under_ceiling(5);
        let outcome = h
            .feed(vec![
                tool_call("c1", "t"),
                tool_result("c1", false),
                tool_call("c2", "t"),
                tool_result("c2", false),
                text("a third turn"),
            ])
            .await;
        let expected = "the harness opened turn 5, past the 5 turns inference.max_turns allows \
                        this task; earlier attempts of this task used 3 of them";
        match outcome {
            SinkOutcome::TurnBudgetExceeded(RuntimeError::HarnessTurnFailed {
                kind,
                message,
                ..
            }) => {
                assert_eq!(kind, "max-turns");
                assert_eq!(message, expected);
            }
            _ => panic!("a turn past the budget must end the run"),
        }
        let failed = h.of_type("harness_failed").await;
        assert_eq!(failed[0]["message"], expected, "{}", failed[0]);
        let turns: Vec<Json> = h
            .of_type("inference")
            .await
            .into_iter()
            .map(|record| record["turn"].clone())
            .collect();
        assert_eq!(
            turns,
            [3, 4],
            "the turn named is the one after the last recorded"
        );
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

    /// A harness `auth` failure with a turn open records that turn as the failed call it was,
    /// counted with the harness's own counts, ahead of the `harness_failed` line.
    #[tokio::test]
    async fn an_auth_failure_records_the_open_turn_as_a_failed_call() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            text("partial"),
            usage(&[("in", 10), ("out", 5)]),
            Event::TurnFailed(TurnFailure {
                kind: FailureKind::Auth,
                message: "not signed in".into(),
            }),
        ])
        .await;
        let events = h.events().await;
        let inference = h.of_type("inference").await;
        assert_eq!(inference.len(), 1);
        let record = &inference[0];
        assert_eq!(record["turn"], 0);
        assert_eq!(record["decision"], "error");
        assert_eq!(record["stop_reason"], "error");
        assert_eq!(record["error_code"], "harness_auth");
        assert_eq!(record["error"], "not signed in");
        assert_eq!(record["input_tokens"], 10);
        assert_eq!(record["output_tokens"], 5);
        assert!(record.get("provider_status").is_none());
        assert_eq!(h.trace.task_turns(), 1, "the open turn stays counted");
        let position = |event_type: &str| {
            events
                .iter()
                .position(|event| event["event_type"] == event_type)
                .unwrap()
        };
        assert!(position("inference") < position("harness_failed"));
        assert_eq!(h.of_type("harness_failed").await[0]["kind"], "auth");
    }

    /// A harness failure with no turn open records one failed call that counts nothing.
    #[tokio::test]
    async fn a_quota_failure_with_no_open_turn_records_an_uncounted_call() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::TurnFailed(TurnFailure {
            kind: FailureKind::Quota,
            message: "out of credit".into(),
        })])
        .await;
        let inference = h.of_type("inference").await;
        assert_eq!(inference.len(), 1);
        assert_eq!(inference[0]["turn"], 0);
        assert_eq!(inference[0]["error_code"], "harness_quota");
        assert!(inference[0].get("input_tokens").is_none());
        assert!(inference[0].get("output_tokens").is_none());
        assert!(inference[0].get("provider_status").is_none());
        assert_eq!(h.trace.task_turns(), 0);
        assert_eq!(h.of_type("harness_failed").await.len(), 1);
    }

    /// A budget or a cancel is not a failed model call: its open turn closes as an ordinary one.
    #[tokio::test]
    async fn max_turns_and_canceled_failures_record_no_failed_call() {
        for kind in [FailureKind::MaxTurns, FailureKind::Canceled] {
            let mut h = Harness::new(10).await;
            h.feed(vec![
                text("partial"),
                Event::TurnFailed(TurnFailure {
                    kind,
                    message: "stopped".into(),
                }),
            ])
            .await;
            let inference = h.of_type("inference").await;
            assert_eq!(inference.len(), 1, "{kind:?}");
            assert_eq!(inference[0]["decision"], "end_turn", "{kind:?}");
            assert!(inference[0].get("error_code").is_none(), "{kind:?}");
            assert!(inference[0].get("provider_status").is_none(), "{kind:?}");
        }
    }

    /// A harness that goes quiet mid-turn leaves its open turn recorded as a failed call.
    #[tokio::test]
    async fn an_inactive_harness_records_its_open_turn_as_a_failed_call() {
        let mut h = Harness::new(10).await;
        h.feed_then_go_quiet(vec![text("thinking"), usage(&[("in", 4), ("out", 2)])])
            .await;
        let inference = h.of_type("inference").await;
        assert_eq!(inference.len(), 1);
        assert_eq!(inference[0]["turn"], 0);
        assert_eq!(inference[0]["decision"], "error");
        assert_eq!(inference[0]["error_code"], "harness_inactive");
        assert_eq!(inference[0]["error"], "the harness went quiet");
        assert_eq!(inference[0]["input_tokens"], 4);
        assert!(inference[0].get("provider_status").is_none());
        assert_eq!(h.trace.task_turns(), 1);
    }

    /// Nothing after the first terminal event in a batch is acted on: the run is over. Each
    /// ignored event is one note naming it.
    #[tokio::test]
    async fn events_after_a_terminal_event_are_ignored() {
        let mut h = Harness::new(10).await;
        let outcome = h
            .feed(vec![
                text("first"),
                Event::TurnEnd("first".into()),
                text("second"),
                Event::TurnEnd("second".into()),
            ])
            .await;
        assert!(matches!(outcome, SinkOutcome::Ended));
        assert_eq!(h.of_type("inference").await.len(), 1);
        assert_eq!(
            std::fs::read_to_string(h.workdir.join("out/result.txt")).unwrap(),
            "first"
        );
        let notes: Vec<Json> = h
            .of_type("harness_note")
            .await
            .into_iter()
            .map(|note| note["text"].clone())
            .collect();
        assert_eq!(
            notes,
            [
                "text after the turn's terminal event, ignored",
                "turn-end after the turn's terminal event, ignored"
            ]
        );
    }

    fn quota_window(name: &str, used: Option<f64>) -> QuotaWindow {
        QuotaWindow {
            name: name.to_string(),
            used,
            resets_at: None,
            window_minutes: None,
            state: None,
        }
    }

    /// A refused window is named, with why, in one note and dropped; the rest stand, and of two
    /// windows with one name the last does, in the first one's place.
    #[tokio::test]
    async fn quota_refuses_malformed_windows_and_keeps_the_last_of_a_name() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::Quota(vec![
            quota_window("five-hour", Some(0.2)),
            quota_window("Five Hour", Some(0.3)),
            quota_window("", None),
            quota_window("-dash", None),
            quota_window(&"a".repeat(65), None),
            quota_window("seven-day", Some(1.5)),
            quota_window("nan", Some(f64::NAN)),
            quota_window("weekly", Some(-0.1)),
            quota_window("monthly", None),
            QuotaWindow {
                state: Some(QuotaState::Warning),
                window_minutes: Some(300),
                ..quota_window("five-hour", Some(0.9))
            },
        ])])
        .await;
        let quota = h.of_type("harness_quota").await;
        assert_eq!(quota.len(), 1);
        assert_eq!(
            quota[0]["windows"],
            serde_json::json!([
                {"name": "five-hour", "used": 0.9, "window_minutes": 300, "state": "warning"},
                {"name": "monthly"}
            ])
        );
        let notes = h.of_type("harness_note").await;
        assert_eq!(notes.len(), 1);
        let note = notes[0]["text"].as_str().unwrap();
        for refused in [
            "\"Five Hour\": the name is not 1 to 64 characters",
            "\"\": the name",
            "\"-dash\": the name",
            &format!("\"{}\": the name", "a".repeat(64)),
            "\"seven-day\": used 1.5 is not a fraction from 0 to 1",
            "\"nan\": used NaN is not a fraction",
            "\"weekly\": used -0.1 is not a fraction",
        ] {
            assert!(note.contains(refused), "{refused}: {note}");
        }
        assert_eq!(
            h.frames()
                .iter()
                .filter(|frame| frame_of(frame)["frame"] == "quota")
                .map(frame_of)
                .collect::<Vec<_>>(),
            [serde_json::json!({"frame": "quota", "windows": [
                {"name": "five-hour", "used": 0.9, "windowMinutes": 300, "state": "warning"},
                {"name": "monthly"}
            ]})]
        );
    }

    /// A report every window of which is refused records nothing but the note, and an empty
    /// suggestion records nothing at all.
    #[tokio::test]
    async fn quota_with_no_valid_window_and_an_empty_suggestion_record_nothing() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::Quota(vec![quota_window("BAD", None)]),
            Event::Quota(Vec::new()),
            Event::SuggestedPrompt(String::new()),
        ])
        .await;
        assert!(h.of_type("harness_quota").await.is_empty());
        assert!(h.of_type("harness_suggestion").await.is_empty());
        assert_eq!(h.of_type("harness_note").await.len(), 1);
        assert!(h.frame_kinds().is_empty(), "{:?}", h.frame_kinds());
    }

    /// Neither new event opens a turn or a segment, and neither is observable work: a run that only
    /// reported a quota and a suggestion commits no harness session.
    #[tokio::test]
    async fn quota_and_suggestion_open_no_turn_and_commit_no_session() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::Quota(vec![quota_window("five-hour", Some(0.5))]),
            Event::SuggestedPrompt("and then?".into()),
        ])
        .await;
        assert_eq!(h.carried, None);
        assert!(h.map.get(CONTEXT).is_none());
        assert_eq!(h.trace.task_turns(), 0);
        assert!(h.of_type("inference").await.is_empty());
        assert_eq!(h.frame_kinds(), ["quota", "suggested-prompt"]);
        let suggestion = h.of_type("harness_suggestion").await;
        assert_eq!(suggestion[0]["text"], "and then?");
    }

    /// An exhausted window is recorded and forwarded, and the turn runs as it would have: no
    /// warning, no failure, the answer recorded.
    #[tokio::test]
    async fn an_exhausted_quota_is_never_acted_on() {
        let mut h = Harness::new(10).await;
        let outcome = h
            .feed(vec![
                Event::Quota(vec![QuotaWindow {
                    state: Some(QuotaState::Exhausted),
                    ..quota_window("five-hour", Some(1.0))
                }]),
                text("answer"),
                Event::TurnEnd("answer".into()),
            ])
            .await;
        assert!(matches!(outcome, SinkOutcome::Ended));
        assert!(h.of_type("harness_warning").await.is_empty());
        assert!(h.of_type("harness_failed").await.is_empty());
        assert_eq!(h.of_type("inference").await.len(), 1);
        assert_eq!(
            h.of_type("harness_quota").await[0]["windows"][0]["state"],
            "exhausted"
        );
        assert_eq!(
            std::fs::read_to_string(h.workdir.join("out/result.txt")).unwrap(),
            "answer"
        );
    }

    /// What arrives while the harness exits: a quota, a suggestion and a note are taken; a late
    /// text, usage and second answer are one note each and change nothing — the result, the one
    /// `inference` record and its counts stay the terminal turn's.
    #[tokio::test]
    async fn after_terminal_takes_quota_suggestion_and_note_and_notes_the_rest() {
        let mut h = Harness::new(10).await;
        let outcome = h
            .feed_then_late(
                vec![
                    usage(&[("in", 10), ("out", 2)]),
                    text("answer"),
                    Event::TurnEnd("answer".into()),
                ],
                vec![
                    Event::Quota(vec![quota_window("five-hour", Some(0.4))]),
                    Event::SuggestedPrompt("try the next step".into()),
                    Event::Note("a late line".into()),
                    text("LATE"),
                    usage(&[("in", 9_000), ("out", 9_000)]),
                    Event::TurnEnd("LATE".into()),
                ],
            )
            .await;
        assert!(matches!(outcome, SinkOutcome::Ended));
        let inference = h.of_type("inference").await;
        assert_eq!(inference.len(), 1);
        assert_eq!(inference[0]["input_tokens"], 10);
        assert_eq!(inference[0]["output_tokens"], 2);
        // The spend is what the same turn settles with nothing after its terminal event.
        let mut alone = Harness::new(10).await;
        alone
            .feed(vec![
                usage(&[("in", 10), ("out", 2)]),
                text("answer"),
                Event::TurnEnd("answer".into()),
            ])
            .await;
        assert!(alone.spend.used() > 0);
        assert_eq!(h.spend.used(), alone.spend.used());
        assert_eq!(
            std::fs::read_to_string(h.workdir.join("out/result.txt")).unwrap(),
            "answer"
        );
        assert_eq!(h.of_type("harness_quota").await.len(), 1);
        assert_eq!(
            h.of_type("harness_suggestion").await[0]["text"],
            "try the next step"
        );
        let notes: Vec<Json> = h
            .of_type("harness_note")
            .await
            .into_iter()
            .map(|note| note["text"].clone())
            .collect();
        assert_eq!(
            notes,
            [
                "a late line",
                "text after the turn's terminal event, ignored",
                "usage after the turn's terminal event, ignored",
                "turn-end after the turn's terminal event, ignored"
            ]
        );
        let kinds = h.frame_kinds();
        assert_eq!(&kinds[kinds.len() - 2..], ["quota", "suggested-prompt"]);
        assert_eq!(h.trace.task_turns(), 1);
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
        let artifact = &data_of(&frames, "artifact")[0];
        assert_eq!(artifact["toolName"], "echo-tool");
        assert_eq!(artifact["content"], "done");
        assert_eq!(artifact["toolCallId"], "c1");
        assert_eq!(artifact["isError"], true);
        assert!(artifact["durationMs"].is_number(), "{artifact}");
        assert_eq!(
            artifact["fenceSource"],
            Json::Null,
            "the bridge returns tool output unfenced"
        );
        assert_eq!(artifact["exitCode"], Json::Null);
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

    // ── Hook artifacts on the turn `turn-end` closes ──────────────────────────
    //
    // The harness's `HookRuntime` has no hooks staged, so these drive the stream directly with
    // the artifacts `close_turn` would have handed it.

    fn hook_artifact(hook_name: &str, payload: &str) -> HookArtifact {
        HookArtifact {
            hook_name: hook_name.to_string(),
            payload: payload.to_string(),
        }
    }

    /// A turn that streamed nothing sends its hook artifact before the whole result.
    #[tokio::test]
    async fn hook_artifact_precedes_the_fallback_text() {
        let mut h = Harness::new(10).await;
        h.a2a.open_segment(1).await;
        h.a2a
            .turn_end("the answer", vec![hook_artifact("review", "{}")])
            .await;
        assert_eq!(
            h.frame_kinds(),
            vec!["status:working:inference turn 1", "artifact", "text:final"],
            "{:#?}",
            h.frames()
        );
        assert_eq!(data_of(&h.frames(), "text")[0]["text"], "the answer");
    }

    /// A streamed turn pays its cursor removal first, so the client's streamed item is complete
    /// before the hook artifact lands under it, and sends no second text.
    #[tokio::test]
    async fn hook_artifact_follows_the_cursor_removal() {
        let mut h = Harness::new(10).await;
        h.a2a.open_segment(1).await;
        h.a2a.text_delta("all of it");
        h.a2a
            .turn_end("all of it", vec![hook_artifact("review", "{}")])
            .await;
        assert_eq!(
            h.frame_kinds(),
            vec![
                "status:working:inference turn 1",
                "text:chunk",
                "text:final",
                "artifact",
            ],
            "{:#?}",
            h.frames()
        );
        assert_eq!(data_of(&h.frames(), "text")[1]["text"], "");
    }

    /// Several hooks' artifacts reach the client in the order they were handed over, which is
    /// hook-registration order.
    #[tokio::test]
    async fn hook_artifact_order_is_kept() {
        let mut h = Harness::new(10).await;
        h.a2a
            .turn_end(
                "",
                vec![hook_artifact("first", "1"), hook_artifact("second", "2")],
            )
            .await;
        let names: Vec<Json> = data_of(&h.frames(), "artifact")
            .iter()
            .map(|frame| frame["toolName"].clone())
            .collect();
        assert_eq!(names, vec!["first", "second"], "{:#?}", h.frames());
    }

    /// An interrupted attempt delivers no answer, and a hook's verdict on it is not one either.
    #[tokio::test]
    async fn hook_artifact_is_not_sent_by_an_interrupted_attempt() {
        let mut h = Harness::new(10).await;
        h.a2a.open_segment(1).await;
        h.a2a.mark_interrupted();
        h.a2a
            .turn_end("the answer", vec![hook_artifact("review", "{}")])
            .await;
        assert_eq!(
            h.frame_kinds(),
            vec!["status:working:inference turn 1"],
            "{:#?}",
            h.frames()
        );
    }

    /// A run with no A2A task has nowhere to send a hook artifact, and does not try, even where a
    /// broadcast exists.
    #[tokio::test]
    async fn hook_artifact_without_a_task_writes_nothing() {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let frames = Arc::new(Mutex::new(SseEventBuffer::new(16)));
        let mut a2a = A2aStream::new(
            Some((tx, Arc::clone(&frames))),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        );
        a2a.turn_end("the answer", vec![hook_artifact("review", "{}")])
            .await;
        let mut bare = A2aStream::new(None, None, None, Arc::new(AtomicBool::new(false)));
        bare.turn_end("the answer", vec![hook_artifact("review", "{}")])
            .await;
        assert!(matches!(
            frames.lock().unwrap().replay_from(0),
            crate::streaming::ReplayResult::Complete(frames) if frames.is_empty()
        ));
    }

    /// The frame is `StreamArtifact::hook`'s, key for key: the operator's own hook speaking,
    /// unfenced, with none of a tool call's keys.
    #[tokio::test]
    async fn hook_artifact_frame_is_the_hook_constructor() {
        let mut h = Harness::new(10).await;
        h.a2a
            .turn_end("", vec![hook_artifact("review", r#"{"reviewed":true}"#)])
            .await;
        let frames = h.frames();
        let artifact = &data_of(&frames, "artifact")[0];
        assert_eq!(
            artifact,
            &serde_json::to_value(crate::streaming::MurmurFrame::Artifact(
                StreamArtifact::hook("review".to_string(), r#"{"reviewed":true}"#.to_string())
            ))
            .unwrap()
        );
        assert_eq!(artifact["fenceSource"], Json::Null);
        assert_eq!(artifact["toolCallId"], Json::Null);
    }

    /// The attempt's ending, on the two paths out of a run the harness itself ends. The stream
    /// writes no final status of its own: the task's one final frame is the reopen loop's.
    #[tokio::test]
    async fn an_attempt_records_its_ending_and_writes_no_final_status() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            text("the answer"),
            Event::TurnEnd("the answer".into()),
        ])
        .await;
        let ending = h.a2a.ending(&Ok(crate::agent::AgentLoopExit::Ok));
        assert_eq!(ending.state, crate::a2a::TaskState::Completed);
        assert_eq!(ending.response.as_deref(), Some("the answer"));
        assert_eq!(ending.message, "task completed");
        assert!(
            data_of(&h.frames(), "status")
                .iter()
                .all(|status| status["status"]["state"] == "TASK_STATE_WORKING"),
            "{:#?}",
            h.frames()
        );

        let mut h = Harness::new(10).await;
        h.feed(vec![Event::TurnFailed(TurnFailure {
            kind: FailureKind::Auth,
            message: "not signed in".into(),
        })])
        .await;
        let ending = h.a2a.ending(&Err(RuntimeError::HarnessTurnFailed {
            kind: "auth".into(),
            message: "not signed in".into(),
            origin: "harness".into(),
        }));
        assert!(h.frame_kinds().is_empty(), "{:#?}", h.frames());
        assert_eq!(ending.state, crate::a2a::TaskState::Failed);
        assert!(ending.response.is_none());
        assert!(ending.message.contains("E-RUN-033"), "{}", ending.message);
        assert!(ending.message.contains("auth"), "{}", ending.message);
        assert!(
            ending.message.contains("not signed in"),
            "{}",
            ending.message
        );
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
                let ending = h.a2a.ending(&Ok(crate::agent::AgentLoopExit::Canceled));

                assert!(
                    data_of(&h.frames(), "status")
                        .iter()
                        .all(|status| status["status"]["state"] == "TASK_STATE_WORKING"),
                    "{:#?}",
                    h.frames()
                );
                assert_eq!(ending.state, crate::a2a::TaskState::Canceled);
                assert!(ending.response.is_none());
                assert_eq!(
                    ending.message.contains("the harness was killed"),
                    killed,
                    "{}",
                    ending.message
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
        let ending = h.a2a.ending(&Ok(crate::agent::AgentLoopExit::Ok));
        assert_eq!(
            ending.state,
            crate::a2a::TaskState::Completed,
            "a run with no task still records how it ended"
        );
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

    // ── A tool call being written ─────────────────────────────────────────────

    /// The `input_bytes` of every `tool-call-progress` frame, in order.
    fn progress_sizes(frames: &[Json]) -> Vec<u64> {
        data_of(frames, "tool-call-progress")
            .iter()
            .map(|data| data["inputBytes"].as_u64().expect("inputBytes is a u64"))
            .collect()
    }

    /// A started call writes its start and its size after the segment's `working` status and
    /// before the `artifact` that answers it, all carrying the call's id. The size the complete
    /// call arrives with is written then, though the interval since the first has not passed.
    #[tokio::test]
    async fn a_started_tool_call_writes_its_start_and_size_before_its_artifact() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call_started("c1", "echo-tool"),
            tool_call_progress("c1", 12),
            tool_call_progress("c1", 40),
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            Event::TurnEnd(String::new()),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            [
                "status:working:inference turn 1",
                "tool-call-started",
                "tool-call-progress",
                "tool-call-progress",
                "artifact",
            ]
        );
        let frames = h.frames();
        let started = data_of(&frames, "tool-call-started");
        assert_eq!(
            started[0],
            serde_json::json!({"frame": "tool-call-started", "toolCallId": "c1", "toolName": "echo-tool"})
        );
        assert_eq!(progress_sizes(&frames), [12, 40]);
        let progress = data_of(&frames, "tool-call-progress");
        assert!(progress.iter().all(|data| data["toolCallId"] == "c1"));
        assert_eq!(data_of(&frames, "artifact")[0]["toolCallId"], "c1");
    }

    /// A started call and its progress are not model actions: the same run with and without them
    /// records the same turns.
    #[tokio::test]
    async fn a_started_tool_call_opens_no_turn() {
        let run = |reported: bool| {
            let mut events = vec![text("working on it")];
            if reported {
                events.push(tool_call_started("c1", "echo-tool"));
                events.push(tool_call_progress("c1", 12));
            }
            events.extend([
                tool_call("c1", "echo-tool"),
                tool_result("c1", false),
                text("done"),
                Event::TurnEnd("done".into()),
            ]);
            events
        };
        let mut records = Vec::new();
        for reported in [false, true] {
            let mut h = Harness::new(10).await;
            h.feed(run(reported)).await;
            let inferences: Vec<Json> = h
                .of_type("inference")
                .await
                .into_iter()
                .map(|record| {
                    serde_json::json!([record["turn"], record["decision"], record["tool_name"]])
                })
                .collect();
            records.push((inferences, h.turn));
        }
        assert_eq!(records[0], records[1]);
        assert_eq!(records[0].0.len(), 2, "{:?}", records[0]);
    }

    /// With the budget spent, a started call and its progress leave the run going; the complete
    /// call is the model action, and it is what ends the run.
    #[tokio::test]
    async fn a_started_tool_call_does_not_spend_the_turn_budget() {
        let spent = || vec![tool_call("c0", "t"), tool_result("c0", false)];

        let mut h = Harness::new(1).await;
        let mut events = spent();
        events.extend([tool_call_started("c1", "t"), tool_call_progress("c1", 12)]);
        assert!(matches!(h.feed(events).await, SinkOutcome::Continue));
        assert_eq!(data_of(&h.frames(), "tool-call-started").len(), 1);

        let mut h = Harness::new(1).await;
        let mut events = spent();
        events.extend([
            tool_call_started("c1", "t"),
            tool_call_progress("c1", 12),
            tool_call("c1", "t"),
        ]);
        assert!(matches!(
            h.feed(events).await,
            SinkOutcome::TurnBudgetExceeded(_)
        ));
    }

    /// What the stream cannot believe writes nothing: progress for a call never started, a second
    /// start, a start after the call is complete or answered, a start with no id, and a size no
    /// larger than one already reported.
    #[tokio::test]
    async fn a_tool_call_report_out_of_order_writes_no_frame() {
        let mut h = Harness::new(10).await;
        h.feed(vec![tool_call_progress("c1", 12)]).await;
        assert!(h.frame_kinds().is_empty(), "{:?}", h.frame_kinds());

        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call_started("c1", "echo-tool"),
            tool_call_started("c1", "echo-tool"),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            ["status:working:inference turn 1", "tool-call-started"]
        );

        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_call_started("c1", "echo-tool"),
            tool_call_progress("c1", 12),
        ])
        .await;
        assert_eq!(h.frame_kinds(), ["status:working:inference turn 1"]);

        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            tool_call_started("c1", "echo-tool"),
            tool_call_progress("c1", 12),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            ["status:working:inference turn 1", "artifact"]
        );

        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call_started("", "echo-tool"),
            tool_call_progress("", 12),
        ])
        .await;
        assert!(h.frame_kinds().is_empty(), "{:?}", h.frame_kinds());

        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call_started("c1", "echo-tool"),
            tool_call_progress("c1", 40),
            tool_call_progress("c1", 40),
            tool_call_progress("c1", 12),
            tool_call("c1", "echo-tool"),
        ])
        .await;
        assert_eq!(progress_sizes(&h.frames()), [40]);
    }

    /// A start replaces none of the text the segment streamed, so it pays no cursor removal; the
    /// result that closes the segment does.
    #[tokio::test]
    async fn a_started_tool_call_pays_no_cursor_removal() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            Event::TextDelta("let me write that".into()),
            tool_call_started("c1", "echo-tool"),
        ])
        .await;
        assert_eq!(
            h.frame_kinds(),
            [
                "status:working:inference turn 1",
                "text:chunk",
                "tool-call-started",
            ]
        );
    }

    /// A complete call named differently from its start is recorded as the harness reported the
    /// complete call, and the difference is noted.
    #[tokio::test]
    async fn a_call_completed_under_another_name_is_noted() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call_started("c1", "write_file"),
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
        ])
        .await;
        let notes = h.of_type("harness_note").await;
        assert_eq!(notes.len(), 1, "{notes:#?}");
        let note = notes[0]["text"].as_str().unwrap();
        assert!(note.contains("c1"), "{note}");
        assert!(note.contains("'write_file'"), "{note}");
        assert!(note.contains("'echo-tool'"), "{note}");
        assert_eq!(data_of(&h.frames(), "artifact")[0]["toolName"], "echo-tool");
    }

    /// Hundreds of sizes in one batch write a handful of frames, so the task's first frames stay
    /// in the replay buffer.
    #[tokio::test]
    async fn a_burst_of_progress_writes_a_bounded_number_of_frames() {
        let mut h = Harness::new(10).await;
        let mut events = vec![tool_call_started("c1", "echo-tool")];
        events.extend((1..=350).map(|i| tool_call_progress("c1", i * 20)));
        events.push(tool_call("c1", "echo-tool"));
        h.feed(events).await;
        let sizes = progress_sizes(&h.frames());
        assert!(
            (1..=2).contains(&sizes.len()),
            "{} progress frames: {sizes:?}",
            sizes.len()
        );
        assert_eq!(sizes.last(), Some(&7000));
        let replay = h.frames.lock().unwrap().replay_from(0);
        match replay {
            crate::streaming::ReplayResult::Complete(frames) => {
                assert_eq!(frames[0].id, 1);
            }
            crate::streaming::ReplayResult::WithGap { .. } => {
                panic!("the burst evicted the task's first frames")
            }
        }
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

    /// Each of the seven events that is work, on its own, with no terminal event after it: the
    /// run was interrupted past the window where the harness has a name and no conversation, and
    /// the session it established is kept. A call the model has only begun writing is model
    /// output in that session, like a fragment.
    #[tokio::test]
    async fn harness_session_any_observable_work_commits_the_reported_id() {
        for work in [
            text("an answer"),
            Event::TextDelta("an ".into()),
            Event::Thinking("pondering".into()),
            Event::ThinkingDelta("pond".into()),
            tool_call("c1", "echo-tool"),
            tool_call_started("c1", "echo-tool"),
            tool_call_progress("c1", 12),
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

        let map = Arc::new(HarnessSessionMap::new(
            None,
            &crate::workdir_fs::WorkdirRoot::open(Path::new("/tmp")).unwrap(),
        ));
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
        let map = Arc::new(HarnessSessionMap::new(
            None,
            &crate::workdir_fs::WorkdirRoot::open(Path::new("/tmp")).unwrap(),
        ));
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
        let map = Arc::new(HarnessSessionMap::new(
            None,
            &crate::workdir_fs::WorkdirRoot::open(Path::new("/tmp")).unwrap(),
        ));
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
        let map = Arc::new(HarnessSessionMap::new(
            None,
            &crate::workdir_fs::WorkdirRoot::open(Path::new("/tmp")).unwrap(),
        ));
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

    /// The base64 every report below says its image was sent as.
    const SENT: &str = "iVBORw0KGgoAAAA=";

    fn call_with(id: &str, name: &str, input: &str) -> Event {
        Event::ToolCall(ToolCallInfo {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        })
    }

    fn result_with(id: &str, output: &str) -> Event {
        Event::ToolResult(ToolResultInfo {
            id: id.to_string(),
            output: output.to_string(),
            is_error: false,
        })
    }

    /// The report of a call to `name` with `input` whose tool attached one PNG at `path`, sent as
    /// [`SENT`].
    fn report(name: &str, input: &str, path: &str) -> AttachmentReport {
        let sha256 = "ab".repeat(32);
        AttachmentReport {
            tool_name: name.to_string(),
            input_json: input.to_string(),
            trace: vec![crate::trace::AttachmentRecord {
                path: path.to_string(),
                media_type: "image/png".to_string(),
                bytes: Some(11),
                sha256: Some(sha256.clone()),
                refused: None,
            }],
            images: vec![SentImage {
                base64: SENT.to_string(),
                media_type: "image/png".to_string(),
                bytes: 11,
                sha256,
            }],
        }
    }

    /// The paths a `tool_call` record's `attachments` name, or `None` when it has no such key.
    fn attached_paths(record: &Json) -> Option<Vec<String>> {
        let attachments = record.get("attachments")?.as_array()?;
        Some(
            attachments
                .iter()
                .map(|entry| entry["path"].as_str().unwrap().to_string())
                .collect(),
        )
    }

    /// A report lands on the call with its bare name and arguments, not on a call of the same tool
    /// with other arguments.
    #[tokio::test]
    async fn a_report_lands_on_the_call_with_its_name_and_arguments() {
        let mut h = Harness::new(10).await;
        h.feed_with_reports(
            vec![report("t", r#"{"a":1}"#, "one.png")],
            vec![
                call_with("c1", "t", r#"{"a":2}"#),
                call_with("c2", "t", r#"{ "a": 1 }"#),
                result_with("c1", "done"),
                result_with("c2", "done"),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["tool_call_id"], "c1");
        assert_eq!(attached_paths(&calls[0]), None);
        assert_eq!(calls[1]["tool_call_id"], "c2");
        assert_eq!(attached_paths(&calls[1]), Some(vec!["one.png".to_string()]));
        assert_eq!(
            calls[1]["attachments"][0],
            json!({"path": "one.png", "media_type": "image/png", "bytes": 11, "sha256": "ab".repeat(32)})
        );
    }

    #[tokio::test]
    async fn two_reports_with_one_key_are_claimed_oldest_first() {
        let mut h = Harness::new(10).await;
        h.feed_with_reports(
            vec![
                report("t", r#"{"a":1}"#, "first.png"),
                report("t", r#"{"a":1}"#, "second.png"),
            ],
            vec![
                call_with("c1", "t", r#"{"a":1}"#),
                result_with("c1", "done"),
                call_with("c2", "t", r#"{"a":1}"#),
                result_with("c2", "done"),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(
            attached_paths(&calls[0]),
            Some(vec!["first.png".to_string()])
        );
        assert_eq!(
            attached_paths(&calls[1]),
            Some(vec!["second.png".to_string()])
        );
    }

    #[tokio::test]
    async fn a_result_with_no_report_writes_no_attachments_key() {
        let mut h = Harness::new(10).await;
        h.feed(vec![
            tool_call("c1", "echo-tool"),
            tool_result("c1", false),
            Event::TurnEnd(String::new()),
        ])
        .await;
        let calls = h.of_type("tool_call").await;
        assert!(calls[0].get("attachments").is_none(), "{}", calls[0]);
        let frames = h.frames();
        assert_eq!(data_of(&frames, "artifact")[0]["attachments"], json!([]));
    }

    /// A report no result claims writes nothing, and the run's end drops it.
    #[tokio::test]
    async fn an_unclaimed_report_writes_nothing_and_is_gone_after_finish() {
        let mut h = Harness::new(10).await;
        let session = RunSession::new(&h.plan, &h.policy, HARNESS, "fixture-driver");
        let mut sink = ProcessEventSink::new(
            &crate::workdir_fs::WorkdirRoot::open(&h.workdir).unwrap(),
            h.max_turns,
            h.task_max_turns,
            h.first_turn,
            session,
            Arc::clone(&h.spend),
            &mut h.a2a,
            GatewayTable::default(),
        );
        sink.queue_report(report("other", "{}", "unclaimed.png"));
        sink.consume(
            vec![
                tool_call("c1", "echo-tool"),
                tool_result("c1", false),
                Event::TurnEnd(String::new()),
            ],
            &mut h.hooks,
            &mut h.trace,
            &mut h.otel,
        )
        .await;
        assert_eq!(sink.reports.len(), 1);
        sink.finish(&mut h.trace).await;
        assert!(sink.reports.is_empty());
        drop(sink);
        let calls = h.of_type("tool_call").await;
        assert!(calls[0].get("attachments").is_none());
        let trace = std::fs::read_to_string(h.workdir.join("trace.jsonl")).unwrap();
        assert!(!trace.contains("unclaimed.png") && !trace.contains(SENT));
    }

    #[tokio::test]
    async fn past_sixty_four_unclaimed_reports_the_oldest_is_dropped() {
        let mut h = Harness::new(10).await;
        let reports = (0..=MAX_UNCLAIMED_REPORTS)
            .map(|n| report("t", &format!(r#"{{"n":{n}}}"#), &format!("{n}.png")))
            .collect();
        h.feed_with_reports(
            reports,
            vec![
                call_with("c0", "t", r#"{"n":0}"#),
                result_with("c0", "done"),
                call_with("c1", "t", r#"{"n":1}"#),
                result_with("c1", "done"),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(
            attached_paths(&calls[0]),
            None,
            "the 65th dropped the oldest"
        );
        assert_eq!(attached_paths(&calls[1]), Some(vec!["1.png".to_string()]));
    }

    /// A driver that hands back the harness's tool result with the image item's JSON inside it
    /// leaves no image bytes in the record, the hook's count or the frame.
    #[tokio::test]
    async fn with_content_capture_the_sent_base64_is_replaced_where_it_is_recorded() {
        let mut h = Harness::new(10).await.capturing_content().await;
        let output =
            format!("one\n{{\"data\":\"{SENT}\",\"mimeType\":\"image/png\",\"type\":\"image\"}}");
        let placeholder = format!("[image: image/png, 11 bytes, sha256 {}]", "ab".repeat(32));
        let replaced = output.replace(SENT, &placeholder);
        h.feed_with_reports(
            vec![report("t", "{}", "a.png")],
            vec![
                call_with("c1", "t", "{}"),
                result_with("c1", &output),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(calls[0]["output"], replaced);
        assert_eq!(calls[0]["output_bytes"], replaced.len());
        assert_eq!(attached_paths(&calls[0]), Some(vec!["a.png".to_string()]));
        let frames = h.frames();
        let artifact = &data_of(&frames, "artifact")[0];
        assert_eq!(artifact["content"], replaced);
        assert_eq!(artifact["attachments"], json!([]));
        let trace = std::fs::read_to_string(h.workdir.join("trace.jsonl")).unwrap();
        assert!(!trace.contains(SENT), "the trace holds the image bytes");
    }

    #[tokio::test]
    async fn an_output_without_the_sent_base64_is_recorded_as_given() {
        let mut h = Harness::new(10).await.capturing_content().await;
        h.feed_with_reports(
            vec![report("t", "{}", "a.png")],
            vec![
                call_with("c1", "t", "{}"),
                result_with("c1", "the harness kept only the text"),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(calls[0]["output"], "the harness kept only the text");
        assert_eq!(
            calls[0]["output_bytes"],
            "the harness kept only the text".len()
        );
        assert_eq!(attached_paths(&calls[0]), Some(vec!["a.png".to_string()]));
    }

    /// Two calls with one name and one set of arguments whose answers were written in the other
    /// order than the harness reports their results: each result is paired with the report whose
    /// image it repeats, so neither record holds image bytes or names the other call's image.
    #[tokio::test]
    async fn a_result_repeating_a_later_reports_image_claims_that_report() {
        let mut h = Harness::new(10).await.capturing_content().await;
        let sent = |base64: &str, path: &str| {
            let mut report = report("t", "{}", path);
            report.images[0].base64 = base64.to_string();
            report
        };
        let (first, second) = ("Rmlyc3RJbWFnZQ==", "U2Vjb25kSW1hZ2U=");
        h.feed_with_reports(
            // The second call's answer was written first.
            vec![sent(second, "second.png"), sent(first, "first.png")],
            vec![
                call_with("c1", "t", "{}"),
                call_with("c2", "t", "{}"),
                result_with("c1", &format!("{{\"data\":\"{first}\"}}")),
                result_with("c2", &format!("{{\"data\":\"{second}\"}}")),
                Event::TurnEnd(String::new()),
            ],
        )
        .await;
        let calls = h.of_type("tool_call").await;
        assert_eq!(
            attached_paths(&calls[0]),
            Some(vec!["first.png".to_string()])
        );
        assert_eq!(
            attached_paths(&calls[1]),
            Some(vec!["second.png".to_string()])
        );
        let trace = std::fs::read_to_string(h.workdir.join("trace.jsonl")).unwrap();
        assert!(
            !trace.contains(first) && !trace.contains(second),
            "the trace holds image bytes"
        );
    }
}
