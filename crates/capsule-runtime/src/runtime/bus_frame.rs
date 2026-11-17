//! The frame that answers a code mode program's `murmur:bus` requests: the dispatch of the
//! model's own call to the code mode tool on the http agent loop, while that call runs in the
//! foreground.
//!
//! One loop polls, side by side: the code mode call itself, its `code_mode.detach_after_secs`
//! timer, the program's next bus request, the operations of the request being served, and every
//! program the code mode call or an operation puts to the session's decision point through
//! `murmur:process`. An operation is never awaited inside the loop's arms: the request being
//! served is a set of futures the loop polls, so a gate request an operation makes is answered
//! while it runs.
//!
//! Each operation meets the checks the model's own call to that tool meets, through the same
//! functions, is run by the same dispatch, fenced as the model's result is, and observed and
//! recorded as the model's call is ([`crate::agent::observe_dispatched_call`]), marked with the
//! code mode call it was made in.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{
    fence_and_label, murmur, AttachedCodeModeCall, CapsuleStoreState, CredentialGateway,
    StartedCodeModeCall,
};
use crate::agent::{observe_dispatched_call, CallGate, ObservedCall, ObservedResult, RanCall};
use crate::bus::{
    refusal, BusCall, BusFrame, BusReply, BusRequest, CallError, CallOrigin, CallRequest,
    CodeModeBus, Operation,
};
use crate::errors::RuntimeError;
use crate::session_process::{GateSender, ProcessGateRequest, ProcessRefusal};
use crate::trace::CodeModeMark;
use crate::types::DispatchOutcome;

type ToolInput = murmur::tool::run::ToolInput;
type ToolResult = murmur::tool::run::ToolResult;

/// One operation's dispatch, answering with its position in the request.
/// Not `Send`: it borrows the session, which the agent loop drives on its own task.
type OperationRun<'s> = Pin<Box<dyn Future<Output = (usize, FinishedOperation)> + 's>>;

/// An operation the shared dispatch returned from.
struct FinishedOperation {
    request: CallRequest,
    input: Value,
    duration_ms: u64,
    /// The outcome after the fence, or the dispatch's failure text.
    result: Result<DispatchOutcome, String>,
}

/// The request being served: its reply, the answers so far by position, and the operations still
/// running.
struct Serving<'s> {
    reply: oneshot::Sender<BusReply>,
    batch: bool,
    answers: Vec<Option<Result<ToolResult, CallError>>>,
    running: FuturesUnordered<OperationRun<'s>>,
}

impl Serving<'_> {
    /// Sends the answers, in input order. A program that went away meanwhile is sent nothing.
    fn answer(self) {
        let mut answers = self.answers.into_iter().map(|answer| {
            answer.unwrap_or_else(|| {
                Err(CallError::Failed(
                    "the operation returned no answer".to_string(),
                ))
            })
        });
        let reply = if self.batch {
            BusReply::Batch(answers.collect())
        } else {
            BusReply::Call(answers.next().unwrap_or_else(|| {
                Err(CallError::Failed(
                    "the operation returned no answer".to_string(),
                ))
            }))
        };
        let _ = self.reply.send(reply);
    }
}

/// The next operation of the request being served to finish; pending while none runs.
async fn next_finished(serving: &mut Option<Serving<'_>>) -> Option<(usize, FinishedOperation)> {
    match serving {
        Some(serving) if !serving.running.is_empty() => serving.running.next().await,
        _ => std::future::pending().await,
    }
}

/// How one call's checks ended.
enum Checked {
    Admitted(Value),
    Refused {
        reason: &'static str,
        text: String,
        /// Whether hooks observe the refusal as a failed call: a required-field refusal is, as
        /// the model's is.
        observed: bool,
    },
}

/// Keeps the first session-ending error a served request met.
fn keep_fatal(fatal: &mut Option<RuntimeError>, error: RuntimeError) {
    if fatal.is_none() {
        *fatal = Some(error);
    }
}

impl CapsuleStoreState {
    /// The model's own call to the code mode tool `name` on the http agent loop: run as
    /// [`Self::dispatch_code_mode`] runs a detachable call, with this dispatch answering the
    /// program's `murmur:bus` requests through `gate` while the call is in the foreground.
    ///
    /// A detach that falls due while a request is being served takes effect once that request is
    /// answered, and no dispatched operation is dropped. When this returns, the frame's receiver
    /// is dropped, and the supervisor refuses every later request of the call.
    ///
    /// An operation that ends the session — a `fatal` outcome, a lost guest worker, a record that
    /// could not be written — is answered to the program and returned as the code mode call's
    /// own [`DispatchOutcome::fatal`], which ends the session after the call is recorded.
    pub(super) async fn dispatch_code_mode_framed(
        &self,
        code_mode: &Arc<crate::code_mode::CodeMode>,
        name: &str,
        input: ToolInput,
        gateway: Option<&Arc<CredentialGateway>>,
        gate: &mut CallGate<'_>,
        frame: BusFrame<'_>,
    ) -> Result<DispatchOutcome, String> {
        use crate::session_process::{GateRoute, ProcessAccess, ProcessCallCtx};

        // One route to the decision point, for the code mode call's own programs and every
        // operation's.
        let (process_tx, mut process_rx) = mpsc::unbounded_channel::<ProcessGateRequest>();
        let process_gate: GateSender =
            Arc::new(move |request: ProcessGateRequest| process_tx.send(request).is_ok());
        let task_id = self
            .session_status
            .as_ref()
            .and_then(|status| status.task_id());
        let registry = self.session_processes.clone();
        let process = match &registry {
            None => ProcessAccess::Unavailable(crate::session_process::unavailable::NO_REGISTRY),
            Some(registry) => ProcessAccess::Available(ProcessCallCtx::new(
                Arc::clone(registry),
                name,
                task_id,
                if gate.gates(self) {
                    GateRoute::Route(Arc::clone(&process_gate))
                } else {
                    GateRoute::Ungated
                },
            )),
        };
        let (bus_tx, mut bus_rx) = mpsc::unbounded_channel::<BusRequest>();
        let StartedCodeModeCall { ticket, mut call } = self
            .start_code_mode_call(
                code_mode,
                name,
                input,
                gateway,
                process,
                CodeModeBus::Frame {
                    route: bus_tx,
                    turn: gate.turn(),
                    repl_call_id: frame.repl_call_id.clone(),
                },
            )
            .await?;
        let mut attached = AttachedCodeModeCall {
            code_mode,
            ticket: &ticket,
            armed: true,
        };
        let mut detach_at = code_mode
            .detach_after()
            .map(|after| tokio::time::Instant::now() + after);
        let mut detach_due = false;
        let mut serving: Option<Serving<'_>> = None;
        let mut fatal: Option<RuntimeError> = None;
        let joined = loop {
            tokio::select! {
                joined = &mut call, if serving.is_none() => break joined,
                () = async {
                    match detach_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                }, if detach_at.is_some() => {
                    detach_at = None;
                    detach_due = true;
                }
                Some(request) = bus_rx.recv(), if serving.is_none() => {
                    serving = self
                        .serve_request(gate, &frame, code_mode.tool(), request, &process_gate, &mut fatal)
                        .await;
                }
                Some((index, finished)) = next_finished(&mut serving) => {
                    let answer = self.record_operation(gate, &frame, finished, &mut fatal).await;
                    if let Some(open) = serving.as_mut() {
                        if let Some(slot) = open.answers.get_mut(index) {
                            *slot = Some(answer);
                        }
                    }
                    if serving.as_ref().is_some_and(|open| open.running.is_empty()) {
                        if let Some(done) = serving.take() {
                            done.answer();
                        }
                    }
                }
                Some(request) = process_rx.recv() => {
                    let verdict = self.answer_process_gate(gate, &request).await;
                    let _ = request.reply.send(verdict);
                }
            }
            if detach_due && serving.is_none() {
                detach_due = false;
                if let Some(detached) = code_mode.detach(&ticket).await {
                    attached.armed = false;
                    return Ok(with_fatal(DispatchOutcome::tool(detached), fatal));
                }
                // It finished as the clock ran out; its result is joined next.
            }
        };
        attached.armed = false;
        let result = joined
            .map_err(|error| format!("tool '{name}' call did not complete: {error}"))
            .and_then(|result| result);
        // A sealed root that failed to build for one of the call's programs ends the session, as
        // it does for a shell command.
        if let Some(detail) = registry
            .as_ref()
            .and_then(|registry| registry.take_sealed_failure())
        {
            keep_fatal(
                &mut fatal,
                RuntimeError::SealedRootConstructionFailed { detail },
            );
        }
        match (result, fatal) {
            (result, None) => result.map(DispatchOutcome::tool),
            (result, Some(fatal)) => Ok(with_fatal(
                DispatchOutcome::tool(result.unwrap_or_else(error_result)),
                Some(fatal),
            )),
        }
    }

    /// The decision point's answer for a program the code mode call or an operation is about to
    /// run: the read-only check and the `on-shell` hooks when the session gates calls, nothing
    /// otherwise. A refusal that could not be recorded refuses anyway.
    async fn answer_process_gate(
        &self,
        gate: &mut CallGate<'_>,
        request: &ProcessGateRequest,
    ) -> Option<ProcessRefusal> {
        if !gate.gates(self) {
            return None;
        }
        match gate.check_with_rule(self, &request.resolved).await {
            Ok(refusal) => refusal.map(|refusal| ProcessRefusal {
                rule: refusal.rule,
                reason: refusal.reason,
            }),
            Err(error) => Some(ProcessRefusal {
                rule: crate::session_process::rule::ON_SHELL,
                reason: error.to_string(),
            }),
        }
    }

    /// What `list` returns now: the session's tool array as the model's would be built at this
    /// moment, before `code_mode.tools_visible` limits it.
    fn bus_operations(&self, frame: &BusFrame<'_>, tool: &str) -> Vec<Operation> {
        let inventory = crate::agent::inventory::build_tool_inventory(
            &self.runtime_root,
            frame.system_prompt_artifact.as_deref(),
            &self.installed_artifacts,
            None,
        );
        crate::bus::operations(&inventory, tool)
    }

    /// Takes one request: answers `list` and an empty batch at once; checks each call in input
    /// order, recording each refusal; and starts every admitted call. `None` when nothing is left
    /// running, the request answered.
    async fn serve_request<'s>(
        &'s self,
        gate: &mut CallGate<'_>,
        frame: &BusFrame<'_>,
        tool: &str,
        request: BusRequest,
        process_gate: &GateSender,
        fatal: &mut Option<RuntimeError>,
    ) -> Option<Serving<'s>> {
        let BusRequest { call, reply } = request;
        let (requests, batch) = match call {
            BusCall::List => {
                let _ = reply.send(BusReply::List(Ok(self.bus_operations(frame, tool))));
                return None;
            }
            BusCall::Call(request) => (vec![request], false),
            BusCall::Batch(requests) => (requests, true),
        };
        let mut serving = Serving {
            reply,
            batch,
            answers: Vec::with_capacity(requests.len()),
            running: FuturesUnordered::new(),
        };
        let operations = self.bus_operations(frame, tool);
        let origin = CallOrigin::CodeMode {
            repl_call_id: frame.repl_call_id.clone(),
        };
        for (index, request) in requests.into_iter().enumerate() {
            match self
                .check_bus_call(gate, &operations, tool, &request, fatal)
                .await
            {
                Checked::Admitted(input) => {
                    serving.answers.push(None);
                    serving.running.push(self.run_operation(
                        index,
                        request,
                        input,
                        frame.cancel.clone(),
                        Arc::clone(process_gate),
                        origin.clone(),
                    ));
                }
                Checked::Refused {
                    reason,
                    text,
                    observed,
                } => {
                    self.record_refusal(gate, frame, &request, reason, &text, observed, fatal)
                        .await;
                    serving.answers.push(Some(Err(CallError::Refused(text))));
                }
            }
        }
        if serving.running.is_empty() {
            serving.answer();
            return None;
        }
        Some(serving)
    }

    /// One call's checks, in order, each final: listed, input is JSON, required fields, then the
    /// decision point when the session gates calls.
    async fn check_bus_call(
        &self,
        gate: &mut CallGate<'_>,
        operations: &[Operation],
        tool: &str,
        request: &CallRequest,
        fatal: &mut Option<RuntimeError>,
    ) -> Checked {
        let name = request.name.as_str();
        let refused = |reason, text| Checked::Refused {
            reason,
            text,
            observed: false,
        };
        if name == tool {
            return refused(
                refusal::CODE_MODE_TOOL,
                crate::bus::code_mode_tool_text(name),
            );
        }
        if crate::bus::NOT_SERVED.contains(&name) {
            return refused(refusal::NOT_SERVED, crate::bus::not_served_text(name));
        }
        if !operations.iter().any(|operation| operation.name == name) {
            return refused(refusal::NOT_LISTED, crate::bus::not_listed_text(name));
        }
        let input: Value = match serde_json::from_str(&request.input) {
            Ok(input) => input,
            Err(error) => {
                return refused(
                    refusal::INPUT_NOT_JSON,
                    crate::bus::input_not_json_text(name, &error),
                )
            }
        };
        match gate.check_required_fields(self, name, None, &input).await {
            Ok(None) => {}
            Ok(Some(text)) => {
                return Checked::Refused {
                    reason: refusal::REQUIRED_FIELDS,
                    text,
                    observed: true,
                }
            }
            Err(error) => {
                let text = error.to_string();
                keep_fatal(fatal, error);
                return refused(refusal::REQUIRED_FIELDS, text);
            }
        }
        if gate.gates(self) {
            let resolved = self.resolve_call(
                name,
                &ToolInput {
                    data: Some(request.input.clone()),
                    log_path: None,
                },
            );
            match gate.check_with_rule(self, &resolved).await {
                Ok(None) => {}
                Ok(Some(refusal)) => return refused(refusal.rule, refusal.text),
                Err(error) => {
                    let text = error.to_string();
                    keep_fatal(fatal, error);
                    return refused(resolved.event_name(), text);
                }
            }
        }
        Checked::Admitted(input)
    }

    /// One admitted call, through the shared dispatch and the fence, as the model's own call to
    /// that tool is run.
    fn run_operation<'s>(
        &'s self,
        index: usize,
        request: CallRequest,
        input: Value,
        canceled: Option<crate::cancel::CancelSignal>,
        process_gate: GateSender,
        origin: CallOrigin,
    ) -> OperationRun<'s> {
        Box::pin(async move {
            let started = Instant::now();
            let mut result = self
                .dispatch_agent_tool_unfenced(
                    &request.name,
                    ToolInput {
                        data: Some(request.input.clone()),
                        log_path: None,
                    },
                    None,
                    None,
                    canceled,
                    Some(process_gate),
                    false,
                    &origin,
                    None,
                )
                .await;
            if let Ok(outcome) = result.as_mut() {
                fence_and_label(&request.name, &self.artifact_origin(&request.name), outcome);
            }
            let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
            (
                index,
                FinishedOperation {
                    request,
                    input,
                    duration_ms,
                    result,
                },
            )
        })
    }

    /// Observes and records a finished operation as the model's call to that tool is, and returns
    /// the program's answer: the post-fence `tool-result`, or the dispatch's failure text.
    async fn record_operation(
        &self,
        gate: &mut CallGate<'_>,
        frame: &BusFrame<'_>,
        finished: FinishedOperation,
        fatal: &mut Option<RuntimeError>,
    ) -> Result<ToolResult, CallError> {
        let FinishedOperation {
            request,
            input,
            duration_ms,
            result,
        } = finished;
        let mark = CodeModeMark {
            repl_call_id: frame.repl_call_id.clone(),
            refusal: None,
        };
        let turn = gate.turn();
        let (hooks, trace, records_root) = gate.parts();
        let call = |result| ObservedCall {
            tool_name: &request.name,
            tool_call_id: None,
            input: &input,
            input_json: &request.input,
            input_bytes: request.input.len() as u64,
            duration_ms,
            mark: Some(&mark),
            result,
        };
        match result {
            Err(text) => {
                if let Err(error) = observe_dispatched_call(
                    hooks,
                    trace,
                    frame.otel,
                    records_root,
                    self,
                    turn,
                    call(ObservedResult::Failed(&text)),
                )
                .await
                {
                    keep_fatal(fatal, error);
                }
                Err(CallError::Failed(text))
            }
            Ok(mut outcome) => {
                if let Some(error) = outcome.fatal.take() {
                    keep_fatal(fatal, error);
                }
                let answer = outcome.result.clone();
                let text = outcome
                    .result
                    .data
                    .clone()
                    .or_else(|| outcome.result.summary.clone())
                    .unwrap_or_else(|| "tool returned no data".to_string());
                let ran = RanCall {
                    text: &text,
                    is_error: !matches!(outcome.result.status, murmur::tool::run::Status::Passed),
                    is_skill: outcome.is_skill,
                    state_effect: crate::agent::extract_state_effect(&outcome.result.metadata),
                    resource_id: crate::agent::extract_resource_id(&outcome.result.metadata),
                    runtime_error: outcome.runtime_error.as_deref(),
                    // The tool's attachments reach the program as it named them, neither checked
                    // nor stored.
                    attachments: Vec::new(),
                    shell: outcome.shell.take(),
                    detached: outcome.detached.take(),
                };
                if let Err(error) = observe_dispatched_call(
                    hooks,
                    trace,
                    frame.otel,
                    records_root,
                    self,
                    turn,
                    call(ObservedResult::Ran(Box::new(ran))),
                )
                .await
                {
                    keep_fatal(fatal, error);
                }
                Ok(answer)
            }
        }
    }

    /// Records a call the frame refused: a failed `tool_call` carrying `reason`, observed by
    /// hooks only when `observed`.
    #[allow(clippy::too_many_arguments)]
    async fn record_refusal(
        &self,
        gate: &mut CallGate<'_>,
        frame: &BusFrame<'_>,
        request: &CallRequest,
        reason: &'static str,
        text: &str,
        observed: bool,
        fatal: &mut Option<RuntimeError>,
    ) {
        let mark = CodeModeMark {
            repl_call_id: frame.repl_call_id.clone(),
            refusal: Some(reason),
        };
        let input = crate::bus::input_record(&request.input);
        let turn = gate.turn();
        let (hooks, trace, records_root) = gate.parts();
        let observed_call = ObservedCall {
            tool_name: &request.name,
            tool_call_id: None,
            input: &input,
            input_json: &request.input,
            input_bytes: request.input.len() as u64,
            duration_ms: 0,
            mark: Some(&mark),
            result: if observed {
                ObservedResult::Failed(text)
            } else {
                ObservedResult::Refused(text)
            },
        };
        if let Err(error) = observe_dispatched_call(
            hooks,
            trace,
            frame.otel,
            records_root,
            self,
            turn,
            observed_call,
        )
        .await
        {
            keep_fatal(fatal, error);
        }
    }
}

/// `outcome` carrying `fatal`, when an operation ended the session.
fn with_fatal(mut outcome: DispatchOutcome, fatal: Option<RuntimeError>) -> DispatchOutcome {
    if fatal.is_some() {
        outcome.fatal = fatal;
    }
    outcome
}

/// A failed code mode call's text as a result, for an outcome that carries a session-ending error.
fn error_result(error: String) -> ToolResult {
    ToolResult {
        status: murmur::tool::run::Status::Error,
        summary: None,
        data: Some(error),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
        attachments: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{tool_linker, EventsSink, InputSink, InputWiring};

    fn component(engine: &wasmtime::Engine, wasm: &str) -> wasmtime::component::Component {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../murmur-cli/tests/fixtures/bus-probe/tool")
            .join(wasm);
        wasmtime::component::Component::from_file(engine, path).unwrap()
    }

    fn linker(
        engine: &wasmtime::Engine,
        bus: bool,
    ) -> wasmtime::component::Linker<super::super::ToolStoreState> {
        let events = EventsSink::Local(crate::stream_events::StreamEventsTarget {
            sse: None,
            task: None,
            chunks_emitted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_calls: None,
        });
        tool_linker(
            engine,
            "bus-repl",
            events,
            InputSink::Local(InputWiring::unavailable()),
            bus,
        )
        .unwrap()
    }

    /// A store whose spec does not link the bus cannot instantiate a component importing
    /// `murmur:bus/bus@0.1.0`; the code mode instance's store can, and a tool importing no bus
    /// instantiates either way.
    #[test]
    fn only_a_store_linking_the_bus_instantiates_a_bus_importer() {
        let engine = super::super::build_engine().unwrap();
        let repl = component(&engine, "bus-repl.wasm");
        let error = match linker(&engine, false).instantiate_pre(&repl) {
            Ok(_) => panic!("a store without the bus instantiated bus-repl"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(crate::bus::BUS_IFACE), "{error}");
        assert!(linker(&engine, true).instantiate_pre(&repl).is_ok());
        let echo = component(&engine, "bus-echo.wasm");
        assert!(linker(&engine, false).instantiate_pre(&echo).is_ok());
        assert!(linker(&engine, true).instantiate_pre(&echo).is_ok());
    }
}
