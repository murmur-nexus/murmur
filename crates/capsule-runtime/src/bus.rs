//! `murmur:bus`: the capsule's operations, served to a program running in the code mode tool's
//! instance.
//!
//! The one WASM tool `code_mode.repl` names imports `murmur:bus/bus@0.1.0` and, from inside a
//! call, lists the operations it may call, calls one, or calls several at once. Every operation is
//! a tool the capsule's model could call, put to the same checks and run by the same dispatch as
//! the model's own call to it: this module holds the interface's types, the texts and reasons of
//! its refusals, the link one code mode call's host carries, and the store side of the import.
//! The frame that answers a call's requests is `crate::runtime::bus_frame`.
//!
//! The bus is answered only while the model's own call to the code mode tool runs in the
//! foreground on `inference.transport: http`. A plan step's call, a call made through a
//! process-transport harness, and a call moved to the background are refused by the supervisor,
//! which records each refused call.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::HasData;

use crate::bindings::code_mode_host::murmur::bus::bus as wit;
use crate::bindings::host::murmur::tool::run::ToolResult;
use crate::code_mode::{CallTicket, CodeMode};
use crate::trace::ResourceTraceAppender;

pub(crate) use wit::{CallError, CallRequest, Operation};

/// The instance name the interface is linked under.
pub(crate) const BUS_IFACE: &str = "murmur:bus/bus@0.1.0";

/// The package an artifact other than the code mode tool may not import, at any version.
pub(crate) const BUS_PACKAGE: &str = "murmur:bus";

/// The `origin` a `tool_call` or `skill_call` record a code mode program caused carries.
pub(crate) const ORIGIN_CODE_MODE: &str = "code_mode";

/// The runtime-provided tools a code mode program may not call: `pull-artifact` and
/// `remove-artifact` change the session's tool set and need the session mutably, which the shared
/// dispatch cannot have while the code mode call holds it; `submit-plan` takes the decision point
/// mutably for every step, which the bus holds while it serves the batch the plan would run in.
pub(crate) const NOT_SERVED: &[&str] = &[
    crate::runtime::PULL_ARTIFACT_TOOL,
    crate::runtime::REMOVE_ARTIFACT_TOOL,
    crate::runtime::SUBMIT_PLAN_TOOL,
];

/// Why the runtime did not run a code mode program's call: a refused call's `refusal` record
/// field. A refusal by the decision point carries the refusing rule instead
/// ([`crate::agent::PROTECTED_PATH_RULE`], `on-tool-call` or `on-shell`).
pub(crate) mod refusal {
    /// The name is not an operation [`super::operations`] lists.
    pub(crate) const NOT_LISTED: &str = "not-listed";
    /// The name is the code mode tool the program runs in.
    pub(crate) const CODE_MODE_TOOL: &str = "code-mode-tool";
    /// The name is one of [`super::NOT_SERVED`].
    pub(crate) const NOT_SERVED: &str = "not-served";
    /// The input text is not JSON.
    pub(crate) const INPUT_NOT_JSON: &str = "input-not-json";
    /// The input lacks a field the tool's `input_schema` requires.
    pub(crate) const REQUIRED_FIELDS: &str = "required-fields";
    /// The code mode call the program runs in was moved to the background.
    pub(crate) const DETACHED: &str = "detached";
    /// The code mode call the program runs in is not the model's own on the http transport.
    pub(crate) const NOT_ANSWERED: &str = "not-answered";
}

/// Who made a call the shared dispatch runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallOrigin {
    /// The model, or a plan step it submitted.
    Model,
    /// A code mode program, through the bus, inside the code mode call `repl_call_id` names.
    CodeMode { repl_call_id: Option<String> },
}

impl CallOrigin {
    /// The code mode call a program's call was made in; `None` for the model's own.
    pub(crate) fn repl_call_id(&self) -> Option<&str> {
        match self {
            Self::Model => None,
            Self::CodeMode { repl_call_id } => repl_call_id.as_deref(),
        }
    }
}

/// One bus function call, as the tools worker sends it to the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum BusCall {
    List,
    Call(CallRequest),
    Batch(Vec<CallRequest>),
}

/// The answer to a [`BusCall`], case for case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum BusReply {
    List(Result<Vec<Operation>, String>),
    Call(Result<ToolResult, CallError>),
    Batch(Vec<Result<ToolResult, CallError>>),
}

// ---------------------------------------------------------------- who may import it

/// Whether `imports`, a component's `murmur:*` import names, name package `murmur:bus` at any
/// version.
pub(crate) fn imports_bus(imports: &[String]) -> bool {
    imports.iter().any(|name| {
        name.split_once('/')
            .map_or(name.as_str(), |(package, _)| package)
            .split('@')
            .next()
            == Some(BUS_PACKAGE)
    })
}

/// Refuses `artifact` when its `imports` name `murmur:bus` and it is not `repl`, the
/// `runtime: tool` artifact `code_mode.repl` names (`None` without a `code_mode` block). Checked
/// before the artifact's fit, so the named tool importing another bus version is refused for its
/// level instead. `mur run` checks it while staging and `mur doctor` on each installed artifact.
pub fn refuse_bus_import(
    artifact: &str,
    imports: &[String],
    repl: Option<&str>,
) -> Result<(), crate::errors::RuntimeError> {
    if imports_bus(imports) && repl != Some(artifact) {
        return Err(crate::errors::RuntimeError::BusImportRefused {
            artifact: artifact.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- texts

/// `not-listed`.
pub(crate) fn not_listed_text(name: &str) -> String {
    format!(
        "'{name}' is not an operation this program may call; list() returns every operation it \
         may call"
    )
}

/// `code-mode-tool`.
pub(crate) fn code_mode_tool_text(name: &str) -> String {
    format!("'{name}' is the code mode tool this program runs in, which a program cannot call")
}

/// `not-served`.
pub(crate) fn not_served_text(name: &str) -> String {
    format!("'{name}' is not served to a code mode program")
}

/// `input-not-json`.
pub(crate) fn input_not_json_text(name: &str, error: &serde_json::Error) -> String {
    format!("the input for '{name}' is not JSON: {error}")
}

/// Why a code mode call's bus requests are refused by the supervisor rather than answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The code mode call is a plan step's.
    PlanStep,
    /// The code mode call was made through a process-transport harness.
    ProcessTransport,
    /// The model's own call on the http transport is no longer in progress, and was not moved to
    /// the background.
    Ended,
}

/// The sentence a request refused outside a frame gets after the leading
/// `'{name}' was not called: `.
fn unanswered_tail(tool: &str, why: Unanswered) -> String {
    let whose = match why {
        Unanswered::PlanStep => "is a plan step's",
        Unanswered::ProcessTransport => "is made through a process-transport harness",
        Unanswered::Ended => "is no longer in progress",
    };
    format!(
        "the bus answers only while the model's own '{tool}' call is in progress on \
         inference.transport: http, and this '{tool}' call {whose}"
    )
}

fn detached_tail(tool: &str, work_id: &str) -> String {
    format!(
        "the '{tool}' call this program runs in was moved to the background as {work_id}, and the \
         bus answers only while that call is in the foreground"
    )
}

/// What a request refused outside a frame is refused with: its reason, and the sentence after
/// `'{name}' was not called: `.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutsideFrame {
    pub(crate) reason: &'static str,
    pub(crate) tail: String,
}

impl OutsideFrame {
    pub(crate) fn unanswered(tool: &str, why: Unanswered) -> Self {
        Self {
            reason: refusal::NOT_ANSWERED,
            tail: unanswered_tail(tool, why),
        }
    }

    pub(crate) fn detached(tool: &str, work_id: &str) -> Self {
        Self {
            reason: refusal::DETACHED,
            tail: detached_tail(tool, work_id),
        }
    }

    /// The text a call to `name` is refused with.
    pub(crate) fn call_text(&self, name: &str) -> String {
        format!("'{name}' was not called: {}", self.tail)
    }
}

// ---------------------------------------------------------------- the frame

/// What the http agent loop hands the dispatch of the model's own call, so that a code mode call
/// it makes can answer its program's bus requests while it runs in the foreground. Every other
/// caller of the shared dispatch hands none.
pub(crate) struct BusFrame<'a> {
    /// The provider's id of the model's call: the bus records' `repl_call_id`.
    pub(crate) repl_call_id: Option<String>,
    /// The attempt's cancel, handed to each operation as the model's own call is handed it.
    pub(crate) cancel: Option<crate::cancel::CancelSignal>,
    /// `inference.system_prompt_artifact`, which the model's tool array leaves out.
    pub(crate) system_prompt_artifact: Option<String>,
    /// The session's span emitter, for the hook inference records an observation flushes.
    pub(crate) otel: &'a crate::otel::OtelEmitter,
}

// ---------------------------------------------------------------- the link

/// One request from a code mode call's program, handed to the frame answering it.
pub(crate) struct BusRequest {
    pub(crate) call: BusCall,
    pub(crate) reply: oneshot::Sender<BusReply>,
}

/// Where one call's bus requests go.
pub(crate) enum BusRoute {
    /// The call's store links no bus: every call but a code mode instance's.
    None,
    /// No frame answers this call's requests; the supervisor refuses each.
    Unanswered(Unanswered),
    /// The frame that dispatched the model's own code mode call, while it runs in the foreground.
    Frame(mpsc::UnboundedSender<BusRequest>),
}

/// Where a code mode call's bus requests are to go, as its dispatch decides it.
pub(crate) enum CodeModeBus {
    /// No frame answers them; the call was made in turn `turn`.
    Unanswered { why: Unanswered, turn: u32 },
    /// The frame holding `route`'s receiver answers them, in turn `turn`, for the model's call
    /// `repl_call_id`.
    Frame {
        route: mpsc::UnboundedSender<BusRequest>,
        turn: u32,
        repl_call_id: Option<String>,
    },
}

impl CodeModeBus {
    /// The link the code mode call `ticket` carries.
    pub(crate) fn link(
        self,
        code_mode: Arc<CodeMode>,
        ticket: CallTicket,
        trace: Option<Arc<ResourceTraceAppender>>,
        task_id: Option<String>,
    ) -> BusLink {
        let (route, turn, repl_call_id) = match self {
            Self::Unanswered { why, turn } => (BusRoute::Unanswered(why), turn, None),
            Self::Frame {
                route,
                turn,
                repl_call_id,
            } => (BusRoute::Frame(route), turn, repl_call_id),
        };
        BusLink::code_mode_call(route, code_mode, ticket, trace, turn, task_id, repl_call_id)
    }
}

/// What a code mode call's host holds of the bus: where its requests go, and what a refusal the
/// supervisor makes itself is recorded with.
pub(crate) struct BusLink {
    route: BusRoute,
    /// Set for a code mode call: the holder, the call's ticket, and the record context.
    call: Option<LinkedCall>,
}

struct LinkedCall {
    code_mode: Arc<CodeMode>,
    ticket: CallTicket,
    trace: Option<Arc<ResourceTraceAppender>>,
    turn: u32,
    task_id: Option<String>,
    repl_call_id: Option<String>,
}

impl BusLink {
    /// The link of a call whose store links no bus.
    pub(crate) fn none() -> Self {
        Self {
            route: BusRoute::None,
            call: None,
        }
    }

    /// The link of the code mode call `ticket`: its requests go to `route`, and a refusal the
    /// supervisor makes is recorded through `trace` in turn `turn` of task `task_id`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn code_mode_call(
        route: BusRoute,
        code_mode: Arc<CodeMode>,
        ticket: CallTicket,
        trace: Option<Arc<ResourceTraceAppender>>,
        turn: u32,
        task_id: Option<String>,
        repl_call_id: Option<String>,
    ) -> Self {
        Self {
            route,
            call: Some(LinkedCall {
                code_mode,
                ticket,
                trace,
                turn,
                task_id,
                repl_call_id,
            }),
        }
    }

    /// Whether this link carries a frame.
    #[cfg(test)]
    pub(crate) fn has_frame(&self) -> bool {
        matches!(self.route, BusRoute::Frame(_))
    }

    /// Answers one request: hands it to the frame, or refuses it when no frame answers.
    pub(crate) async fn answer(&self, call: BusCall) -> BusReply {
        let Some(linked) = &self.call else {
            return refuse_unlinked(call);
        };
        let tool = linked.code_mode.tool().to_string();
        let outside = match &self.route {
            BusRoute::None => return refuse_unlinked(call),
            BusRoute::Unanswered(why) => OutsideFrame::unanswered(&tool, *why),
            BusRoute::Frame(frame) => {
                let (reply, answer) = oneshot::channel();
                let sent = frame.send(BusRequest {
                    call: call.clone(),
                    reply,
                });
                if sent.is_ok() {
                    if let Ok(answered) = answer.await {
                        return answered;
                    }
                }
                match linked.code_mode.work_id_of(&linked.ticket) {
                    Some(work_id) => OutsideFrame::detached(&tool, &work_id),
                    None => OutsideFrame::unanswered(&tool, Unanswered::Ended),
                }
            }
        };
        linked.refuse(call, &outside).await
    }
}

impl LinkedCall {
    /// Refuses `call` with `outside`, recording each refused operation call.
    async fn refuse(&self, call: BusCall, outside: &OutsideFrame) -> BusReply {
        match call {
            BusCall::List => BusReply::List(Err(outside.tail.clone())),
            BusCall::Call(request) => BusReply::Call(Err(self.refuse_one(request, outside).await)),
            BusCall::Batch(requests) => {
                let mut answers = Vec::with_capacity(requests.len());
                for request in requests {
                    answers.push(Err(self.refuse_one(request, outside).await));
                }
                BusReply::Batch(answers)
            }
        }
    }

    async fn refuse_one(&self, request: CallRequest, outside: &OutsideFrame) -> CallError {
        let text = outside.call_text(&request.name);
        if let Some(trace) = &self.trace {
            trace
                .write_bus_refusal(
                    self.turn,
                    self.task_id.clone(),
                    &request.name,
                    input_record(&request.input),
                    request.input.len() as u64,
                    &text,
                    outside.reason,
                    self.repl_call_id.clone(),
                )
                .await;
        }
        CallError::Refused(text)
    }
}

/// A request on a link with no code mode call behind it: unreachable through a linker that links
/// the bus only for such calls, answered rather than trusted.
fn refuse_unlinked(call: BusCall) -> BusReply {
    const TEXT: &str = "this call is not the code mode tool's, so no bus is served to it";
    match call {
        BusCall::List => BusReply::List(Err(TEXT.to_string())),
        BusCall::Call(_) => BusReply::Call(Err(CallError::Refused(TEXT.to_string()))),
        BusCall::Batch(requests) => BusReply::Batch(
            requests
                .iter()
                .map(|_| Err(CallError::Refused(TEXT.to_string())))
                .collect(),
        ),
    }
}

/// A call's input as its record's `input` field: the JSON value, or the text as a JSON string
/// when it is not JSON.
pub(crate) fn input_record(input: &str) -> serde_json::Value {
    serde_json::from_str(input).unwrap_or_else(|_| serde_json::Value::String(input.to_string()))
}

// ---------------------------------------------------------------- what list returns

/// The operations a code mode program may call, built from `inventory` — the session's tool
/// array before `code_mode.tools_visible` limits it, in its order — leaving out the code mode tool
/// `tool` and [`NOT_SERVED`].
pub(crate) fn operations(inventory: &[serde_json::Value], tool: &str) -> Vec<Operation> {
    inventory
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?;
            if name == tool || NOT_SERVED.contains(&name) {
                return None;
            }
            Some(Operation {
                name: name.to_string(),
                description: entry
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input_schema: entry
                    .get("parameters")
                    .map(serde_json::Value::to_string)
                    .unwrap_or_else(|| "{}".to_string()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------- the store side

/// Where a tool store's bus functions are answered.
pub(crate) enum BusSide {
    /// In-process: no frame can reach this store, so every function refuses.
    Local,
    /// In the tools worker: each function is a host call to the supervisor.
    Remote(Arc<crate::guest_worker::RemoteLink>),
}

/// The text every bus function refuses with in a store run in-process.
pub(crate) const IN_PROCESS_TEXT: &str =
    "murmur:bus is served only to the code mode instance in the tools worker";

/// The host-side binding of `murmur:bus` over one tool store.
pub(crate) struct BusView<'a> {
    pub(crate) side: &'a BusSide,
}

/// The [`HasData`] marker `add_to_linker` is instantiated with.
pub(crate) struct BusHost;

impl HasData for BusHost {
    type Data<'a> = BusView<'a>;
}

impl BusView<'_> {
    async fn ask(&self, call: BusCall) -> Result<BusReply, String> {
        match self.side {
            BusSide::Local => Err(IN_PROCESS_TEXT.to_string()),
            BusSide::Remote(link) => link.bus(call).await,
        }
    }
}

impl wit::Host for BusView<'_> {
    async fn list(&mut self) -> Result<Vec<Operation>, String> {
        match self.ask(BusCall::List).await {
            Ok(BusReply::List(answer)) => answer,
            Ok(other) => Err(format!("the supervisor answered list() with {other:?}")),
            Err(reason) => Err(reason),
        }
    }

    async fn call(&mut self, name: String, input: String) -> Result<ToolResult, CallError> {
        match self.ask(BusCall::Call(CallRequest { name, input })).await {
            Ok(BusReply::Call(answer)) => answer,
            Ok(other) => Err(CallError::Refused(format!(
                "the supervisor answered call() with {other:?}"
            ))),
            Err(reason) => Err(CallError::Refused(reason)),
        }
    }

    async fn call_batch(&mut self, calls: Vec<CallRequest>) -> Vec<Result<ToolResult, CallError>> {
        let count = calls.len();
        match self.ask(BusCall::Batch(calls)).await {
            Ok(BusReply::Batch(answers)) if answers.len() == count => answers,
            Ok(other) => vec![
                Err(CallError::Refused(format!(
                    "the supervisor answered call-batch() with {other:?}"
                )));
                count
            ],
            Err(reason) => vec![Err(CallError::Refused(reason)); count],
        }
    }
}

/// Defines `murmur:bus/bus@0.1.0` in `linker`, each function answered through `view`'s side.
pub(crate) fn add_to_linker<T: Send + 'static>(
    linker: &mut wasmtime::component::Linker<T>,
    view: fn(&mut T) -> BusView<'_>,
) -> wasmtime::Result<()> {
    wit::add_to_linker::<T, BusHost>(linker, view)
}

#[cfg(test)]
mod tests;
