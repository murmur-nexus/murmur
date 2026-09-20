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
    time::Instant,
};

use murmur_artifact::{security_warning_link, W_SEC_031};
use serde_json::Value;

use crate::{
    errors::RuntimeError,
    hooks::{HookEvent, HookRuntime},
    otel::OtelEmitter,
    process_driver::{Event, FailureKind},
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
    reported_session: Option<String>,
    /// The A2A half of the same events. Borrowed rather than owned, because the attempt writes
    /// its one terminal status through it after this sink is done with the run.
    a2a: &'a mut A2aStream,
}

impl<'a> ProcessEventSink<'a> {
    pub(super) fn new(
        workdir: &Path,
        max_turns: u32,
        session: RunSession,
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
            a2a,
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

    /// Record every tool call the harness never answered. A call with no result is not a
    /// `tool_call` record: nothing is known about how it went, and inventing a status would put a
    /// tool call in the trace that no tool ever finished.
    pub(super) async fn finish(&mut self, trace: &mut TraceWriter) {
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
    }

    async fn handle(
        &mut self,
        event: Event,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) -> SinkOutcome {
        match event {
            Event::SessionStarted(info) => {
                let _ = trace
                    .write_harness_session(&info.id, &info.auth, info.model.as_deref())
                    .await;
                // Remembered the moment the harness names it, not at the end of the turn: the
                // conversation exists from here on, whatever the turn goes on to do.
                if !info.id.is_empty() {
                    self.reported_session = Some(info.id.clone());
                    self.session.remember(&info.id);
                }
                if info.auth != SUBSCRIPTION_AUTH {
                    let message = format!(
                        "the harness session reports auth '{}', not '{SUBSCRIPTION_AUTH}' — this \
                         run's spend may be billed to an API key, which murmur neither counts nor \
                         limits",
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
                self.close_turn(hooks, trace, otel).await;
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
            Event::Note(text) => {
                let _ = trace.write_harness_note(&text).await;
                SinkOutcome::Continue
            }
            Event::TurnEnd(result) => {
                self.close_turn(hooks, trace, otel).await;
                // A harness that finished a turn without ever naming its session answers to the
                // id it was handed: that is the conversation, and this is the only chance to say
                // so.
                if self.reported_session.is_none() {
                    let id = self.session.id().to_string();
                    self.session.remember(&id);
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
                self.close_turn(hooks, trace, otel).await;
                self.a2a.turn_failed();
                let kind = failure_kind_name(failure.kind);
                let _ = trace
                    .write_harness_failed(kind, &failure.message, "harness")
                    .await;
                // A turn that failed without ever reporting a session established nothing, so
                // nothing is remembered. When it was a resume, that is the harness saying it does
                // not hold the conversation this context names, which is its own failure.
                if let Some(gone) = self.session.session_gone(
                    failure.kind,
                    self.reported_session.is_some(),
                    &failure.message,
                ) {
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

    /// Write the open turn's `inference` record, if one is open. Tokens are zero on this
    /// transport: the harness holds its own conversation and reports no usage the runtime could
    /// record.
    async fn close_turn(
        &mut self,
        hooks: &mut HookRuntime,
        trace: &mut TraceWriter,
        otel: &mut OtelEmitter,
    ) {
        let Some(open) = self.open.take() else {
            return;
        };
        let decision = if open.first_tool.is_some() {
            "tool_call"
        } else {
            "end_turn"
        };
        let _ = trace
            .write_inference(
                open.index,
                0,
                0,
                decision.to_string(),
                // The runtime reads no driver response of its own on this transport, so there is
                // no provider stop reason and no request payload to hash.
                None,
                open.first_tool.clone(),
                None,
                None,
                Vec::new(),
                None,
            )
            .await;
        otel.emit_inference(
            open.index,
            0,
            0,
            decision,
            None,
            open.first_tool.as_deref(),
            0,
            None,
            None,
        )
        .await;
        hooks
            .emit(
                &self.workdir,
                HookEvent::Inference {
                    turn: open.index,
                    input_tokens: 0,
                    output_tokens: 0,
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
    }

    impl Harness {
        async fn new(max_turns: u32) -> Self {
            Self::threading(
                max_turns,
                Arc::new(HarnessSessionMap::new(None, Path::new("/tmp"))),
            )
            .await
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
            }
        }

        /// The sink is built per batch because it borrows the stream; its turn state is a
        /// test-local concern and every test here feeds exactly one batch.
        async fn feed(&mut self, events: Vec<Event>) -> SinkOutcome {
            let session = RunSession::new(&self.plan, &self.policy, HARNESS, "fixture-driver");
            let mut sink =
                ProcessEventSink::new(&self.workdir, self.max_turns, session, &mut self.a2a);
            let outcome = sink
                .consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await;
            sink.finish(&mut self.trace).await;
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

    /// Whatever the harness calls its session is what the context is keyed on from then on.
    #[tokio::test]
    async fn harness_session_the_reported_id_replaces_the_one_the_runtime_minted() {
        let mut h = Harness::new(10).await;
        h.feed(vec![Event::SessionStarted(SessionInfo {
            id: "harness-chose-this".into(),
            auth: SUBSCRIPTION_AUTH.into(),
            model: None,
        })])
        .await;
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some("harness-chose-this"));
        assert_ne!(h.planned_id, "harness-chose-this");
    }

    /// A harness that says nothing about its session answers to the id it was handed.
    #[tokio::test]
    async fn harness_session_a_silent_run_that_ends_remembers_the_minted_id() {
        let mut h = Harness::new(10).await;
        assert_eq!(h.map.get(CONTEXT), None);
        h.feed(vec![Event::TurnEnd("done".into())]).await;
        assert_eq!(h.map.get(CONTEXT).as_deref(), Some(h.planned_id.as_str()));
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
