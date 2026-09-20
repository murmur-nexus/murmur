//! What the runtime does with the typed events a process driver reads out of its harness.
//!
//! The driver turns harness output into [`Event`]s; this module is the other half — it turns
//! those events into everything a turn records: the `inference` and `tool_call` trace events, the
//! `Inference` and `ToolCall` hooks, the OTel spans, the result text, and the harness's own
//! diagnostics. The runtime never reads the harness's output itself, so nothing here interprets
//! text; it only counts turns and writes records.
//!
//! # Turns
//!
//! One logical turn is one model action, counted as the `transport: http` path counts it. A turn
//! opens at the first `text` or `tool-call` since the run started or since the last `tool-result`,
//! and closes at the next `tool-result` or terminal event. Thinking and the streaming deltas never
//! open one: a harness that streams its reasoning as its own events would otherwise burn a turn
//! per thought, and `max_turns` would mean something different on each transport.

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
    trace::TraceWriter,
};

use super::process::RunSession;

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
pub(super) struct ProcessEventSink {
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
}

impl ProcessEventSink {
    pub(super) fn new(workdir: &Path, max_turns: u32, session: RunSession) -> Self {
        Self {
            workdir: workdir.to_path_buf(),
            max_turns,
            turns: 0,
            open: None,
            pending: HashMap::new(),
            finished: false,
            session,
            reported_session: None,
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
                if let Some(open) = self.open.as_mut() {
                    open.text = Some(text);
                }
                SinkOutcome::Continue
            }
            Event::ToolCall(call) => {
                if let Some(exceeded) = self.open_turn(trace).await {
                    return exceeded;
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
                            tool_name: call.name,
                            input_bytes: call.input_bytes,
                            input: call.input.to_string(),
                            output_bytes,
                            duration_ms,
                            status: status.to_string(),
                        },
                    )
                    .await;
                SinkOutcome::Continue
            }
            // Streaming fragments and reasoning. Both are evidence the harness is alive — which
            // the runner already took from the line arriving — and neither is a model action.
            Event::TextDelta(_) | Event::ThinkingDelta(_) | Event::Thinking(_) => {
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
                    Ok(()) => SinkOutcome::Ended,
                    Err(message) => SinkOutcome::Failed(RuntimeError::AgentLoopFailed(message)),
                }
            }
            Event::TurnFailed(failure) => {
                self.close_turn(hooks, trace, otel).await;
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
    use std::sync::Arc;

    use super::*;
    use crate::agent::process::{plan_session, HarnessSessionPolicy};
    use crate::harness_session::HarnessSessionMap;
    use crate::process_driver::{SessionInfo, ToolCallInfo, ToolResultInfo, TurnFailure};
    use serde_json::Value as Json;

    /// The context every sink below runs a task under.
    const CONTEXT: &str = "ctx_test";

    /// The harness name every sink below records with the session it remembers.
    const HARNESS: &str = "fixture-harness";

    /// A sink, its sinks, and the workdir they write into. Held together so a test can drop the
    /// tempdir only after reading `trace.jsonl` back.
    struct Harness {
        _dir: tempfile::TempDir,
        workdir: PathBuf,
        sink: ProcessEventSink,
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
            let session =
                crate::agent::process::RunSession::new(&plan, &policy, HARNESS, "fixture-driver");
            Self {
                _dir: dir,
                workdir: workdir.clone(),
                sink: ProcessEventSink::new(&workdir, max_turns, session),
                hooks,
                trace,
                otel,
                map,
                planned_id,
            }
        }

        async fn feed(&mut self, events: Vec<Event>) -> SinkOutcome {
            self.sink
                .consume(events, &mut self.hooks, &mut self.trace, &mut self.otel)
                .await
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
            Event::Retry(crate::process_driver::RetryInfo {
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

    /// A result nobody called for, and a call nobody answered, are both notes — never a
    /// `tool_call` record for something that did not happen.
    #[tokio::test]
    async fn unpaired_calls_and_results_are_notes() {
        let mut h = Harness::new(10).await;
        h.feed(vec![tool_result("ghost", false), tool_call("c9", "t")])
            .await;
        h.sink.finish(&mut h.trace).await;
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
