//! Code mode: the one `runtime: tool` artifact a manifest names in `code_mode.repl` keeps a single
//! instance in the tools worker for the rest of the session, so a value one call leaves in the
//! guest's memory is there for the next.
//!
//! This module is the supervisor's side of it: which instance the worker holds and how many calls
//! it answered, the call running in it, and the calls that were moved to the background under a
//! work id. The instance itself lives only in the tools worker
//! ([`crate::guest_worker::wire::ToWorker::SessionToolCall`]).
//!
//! The instance is opened by the first call to the tool, never at launch. It runs one call at a
//! time, and ends at the session's end, on a `background-command stop`, on a trap, a deadline or a
//! resource limit, on a call dropped before it was moved to the background, or with the worker.
//! After every end but the session's and the worker's, the next call opens a new instance.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use murmur_artifact::CodeModeConfig;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::background_command::BACKGROUND_COMMAND_TOOL;
use crate::bindings::host::murmur::tool::run::{Status, ToolResult};
use crate::detached::WaitEnd;
use crate::guest_worker::GuestWorkers;
use crate::limits::GuestFailure;
use crate::runtime::{ToolCallFailure, ToolFailureKind};
use crate::trace::ResourceTraceAppender;

/// Why a code mode instance ended, as `code_mode_instance_end.reason` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum EndReason {
    /// The session ended with the instance open.
    SessionEnd,
    /// A `background-command stop` named the call running in it.
    Stopped,
    /// The guest trapped, or the instance could not be built.
    Trap,
    /// The call ran past `capabilities.limits.deadline_seconds`, by the epoch or the wall clock.
    Deadline,
    /// The guest grew past a `capabilities.limits` resource cap.
    ResourceLimit,
    /// The call was dropped before it was moved to the background, as a task cancel drops it.
    Canceled,
    /// The tools worker was lost.
    WorkerLost,
}

impl EndReason {
    /// The value `code_mode_instance_end.reason` carries.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::SessionEnd => "session_end",
            Self::Stopped => "stopped",
            Self::Trap => "trap",
            Self::Deadline => "deadline",
            Self::ResourceLimit => "resource_limit",
            Self::Canceled => "canceled",
            Self::WorkerLost => "worker_lost",
        }
    }

    /// Whether the record of this end carries the failure text.
    fn carries_message(self) -> bool {
        matches!(self, Self::Trap | Self::Deadline | Self::ResourceLimit)
    }
}

/// A call that ended its instance: why, and the failure text the call returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstanceEnd {
    pub(crate) reason: EndReason,
    pub(crate) message: String,
}

impl InstanceEnd {
    /// The end a failed instantiation or call makes: a wall-clock bound or the epoch deadline is
    /// `deadline`, a limiter denial `resource_limit`, anything else `trap`.
    pub(crate) fn from_failure(failure: &ToolCallFailure) -> Self {
        let reason = match &failure.kind {
            ToolFailureKind::WallDeadline
            | ToolFailureKind::Guest(GuestFailure::DeadlineExceeded { .. }) => EndReason::Deadline,
            ToolFailureKind::Guest(GuestFailure::ResourceLimit { .. }) => EndReason::ResourceLimit,
            ToolFailureKind::Guest(GuestFailure::Other) | ToolFailureKind::Setup => EndReason::Trap,
        };
        Self {
            reason,
            message: failure.message.clone(),
        }
    }
}

/// The tools worker's answer to one call on the instance.
#[derive(Debug)]
pub(crate) struct SessionToolReply {
    /// The call's result, or the failure text.
    pub(crate) result: Result<ToolResult, String>,
    /// Set when the call ended the instance; the worker no longer holds it.
    pub(crate) ended: Option<InstanceEnd>,
}

/// The sentence a failure that ended the instance is followed by.
pub(crate) fn instance_ended_sentence(tool: &str) -> String {
    format!(
        "The code mode instance ended; every value it held is lost, and the next call to '{tool}' \
         starts a new instance."
    )
}

/// What `background-command stop` returns after the state lines of a stopped call.
pub(crate) fn stopped_sentence(tool: &str) -> String {
    format!(
        "The call was stopped by ending the code mode instance; every value the instance held is \
         lost, and the next call to '{tool}' starts a new instance."
    )
}

/// Ids for tickets and worker-side instances: unique within the process, so within a worker.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// The session's code mode: built at launch only for a manifest with a `code_mode` block.
pub(crate) struct CodeMode {
    artifact: String,
    tool: String,
    detach_after: Option<Duration>,
    tools_visible: bool,
    workers: Arc<GuestWorkers>,
    trace: Option<Arc<ResourceTraceAppender>>,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for CodeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeMode")
            .field("artifact", &self.artifact)
            .field("tool", &self.tool)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    instance: Option<Instance>,
    /// The sequence number the last opened instance was given.
    sequence: u64,
    running: Option<Running>,
    /// Every call moved to the background this session, by work id. Never pruned: a work id is
    /// valid for the rest of the session.
    detached: HashMap<String, Arc<DetachedCall>>,
    /// Set at the session's end and when the worker is lost: no instance opens after either.
    closed: bool,
}

/// The instance the tools worker holds.
#[derive(Debug, Clone, Copy)]
struct Instance {
    /// The worker's id for it.
    id: u64,
    /// 1 for the session's first instance, 2 for the one after it ended, and so on.
    sequence: u64,
    /// The calls it answered with a result.
    calls: u64,
}

struct Running {
    token: u64,
    started: Instant,
    detached: Option<Arc<DetachedCall>>,
    /// The task the call runs on, aborted to drop the call.
    abort: Option<tokio::task::AbortHandle>,
}

/// A call moved to the background, and its outcome once it has one.
struct DetachedCall {
    started: Instant,
    outcome: watch::Sender<Option<CallOutcome>>,
}

#[derive(Debug, Clone)]
enum CallOutcome {
    Finished {
        result: Result<ToolResult, String>,
        elapsed: Duration,
    },
    Stopped {
        elapsed: Duration,
    },
}

/// One call admitted to the instance, from [`CodeMode::begin`] to [`CodeMode::finish`].
#[derive(Debug, Clone)]
pub(crate) struct CallTicket {
    token: u64,
    /// The worker's id for the instance the call runs on.
    pub(crate) instance: u64,
    sequence: u64,
    task_id: Option<String>,
}

/// A detached call's state, as `background-command` reports it.
#[derive(Debug, Clone)]
pub(crate) struct CallSnapshot {
    pub(crate) work_id: String,
    pub(crate) tool: String,
    pub(crate) state: CallState,
    /// From the call's start to now, or to its end once it has ended.
    pub(crate) elapsed: Duration,
}

/// Where a detached call stands.
#[derive(Debug, Clone)]
pub(crate) enum CallState {
    Running,
    /// The call returned; its own result or failure text.
    Finished(Result<ToolResult, String>),
    Stopped,
}

impl CallState {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished(_) => "finished",
            Self::Stopped => "stopped",
        }
    }
}

impl CodeMode {
    pub(crate) fn new(
        config: &CodeModeConfig,
        workers: Arc<GuestWorkers>,
        trace: Option<Arc<ResourceTraceAppender>>,
    ) -> Self {
        Self {
            artifact: config.repl.clone(),
            tool: murmur_artifact::tool_name_of(&config.repl).to_string(),
            detach_after: config.detach_after_secs.map(Duration::from_secs),
            tools_visible: config.tools_visible,
            workers,
            trace,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The artifact's full `<scope>/<name>`.
    pub(crate) fn artifact(&self) -> &str {
        &self.artifact
    }

    /// The tool name the model calls the artifact by.
    pub(crate) fn tool(&self) -> &str {
        &self.tool
    }

    /// How long a model's call runs before it is moved to the background; `None` never.
    pub(crate) fn detach_after(&self) -> Option<Duration> {
        self.detach_after
    }

    /// The only names the tool array offers, for `code_mode.tools_visible: false`: the code mode
    /// tool, and `background-command` when calls can detach. `None` offers every tool.
    pub(crate) fn offered_only(&self) -> Option<Vec<String>> {
        offered_only(&self.tool, self.tools_visible, self.detach_after.is_some())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admits a call to the instance, opening one when none is held, or refuses it while another
    /// call runs. The call that opens an instance writes its `code_mode_instance_start`.
    pub(crate) async fn begin(&self, task_id: Option<String>) -> Result<CallTicket, String> {
        let (ticket, opened) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(format!(
                    "'{}' cannot be called: the session's code mode instance ended with the \
                     session",
                    self.tool
                ));
            }
            if let Some(running) = &inner.running {
                let secs = running.started.elapsed().as_secs();
                return Err(match &running.detached {
                    Some(call) => format!(
                        "'{}' is running another call ({}, started {secs} s ago); the code mode \
                         instance runs one call at a time",
                        self.tool,
                        call_work_id(&inner, call)
                    ),
                    None => format!(
                        "'{}' is running another call, started {secs} s ago; the code mode \
                         instance runs one call at a time",
                        self.tool
                    ),
                });
            }
            let opened = inner.instance.is_none();
            if opened {
                inner.sequence += 1;
                inner.instance = Some(Instance {
                    id: next_id(),
                    sequence: inner.sequence,
                    calls: 0,
                });
            }
            let instance = inner.instance.expect("an instance is held");
            let token = next_id();
            inner.running = Some(Running {
                token,
                started: Instant::now(),
                detached: None,
                abort: None,
            });
            let ticket = CallTicket {
                token,
                instance: instance.id,
                sequence: instance.sequence,
                task_id,
            };
            (ticket, opened)
        };
        if opened {
            if let Some(trace) = &self.trace {
                trace
                    .write_code_mode_instance_start(
                        ticket.task_id.clone(),
                        &self.artifact,
                        &self.tool,
                        ticket.sequence,
                    )
                    .await;
            }
        }
        Ok(ticket)
    }

    /// Records the task `ticket`'s call runs on, so a stop or a cancel can drop it.
    pub(crate) fn set_abort(&self, ticket: &CallTicket, abort: tokio::task::AbortHandle) {
        let mut inner = self.lock();
        // A call that already finished or was dropped has nothing left to abort.
        if let Some(running) = inner.running.as_mut().filter(|r| r.token == ticket.token) {
            running.abort = Some(abort);
        }
    }

    /// Settles `ticket`'s call with the worker's `reply`: counts an answered call, ends the
    /// instance on a failure that ended it, hands a detached call's outcome to its waiters, and
    /// returns what the model reads. A call a stop or a cancel already settled is returned as is.
    pub(crate) async fn finish(
        &self,
        ticket: &CallTicket,
        reply: Result<SessionToolReply, String>,
    ) -> Result<ToolResult, String> {
        let (result, ended) = {
            let mut inner = self.lock();
            let Some(running) = inner
                .running
                .take_if(|running| running.token == ticket.token)
            else {
                return reply.and_then(|reply| reply.result);
            };
            let elapsed = running.started.elapsed();
            let (result, end) = match reply {
                Err(error) => {
                    let reason = if self.workers.lost().is_some() {
                        inner.closed = true;
                        EndReason::WorkerLost
                    } else {
                        EndReason::Trap
                    };
                    (
                        Err(error.clone()),
                        Some(InstanceEnd {
                            reason,
                            message: error,
                        }),
                    )
                }
                Ok(SessionToolReply { result, ended }) => (result, ended),
            };
            let ended = match end {
                Some(end) => inner.instance.take().map(|instance| (instance, end)),
                None => {
                    if result.is_ok() {
                        if let Some(instance) = inner.instance.as_mut() {
                            instance.calls += 1;
                        }
                    }
                    None
                }
            };
            let result = match (&ended, result) {
                (Some((_, end)), Err(error)) if end.reason != EndReason::WorkerLost => {
                    Err(format!("{error} {}", instance_ended_sentence(&self.tool)))
                }
                (_, result) => result,
            };
            if let Some(call) = &running.detached {
                call.outcome.send_replace(Some(CallOutcome::Finished {
                    result: result.clone(),
                    elapsed,
                }));
            }
            (result, ended)
        };
        if let Some((instance, end)) = ended {
            if end.reason != EndReason::WorkerLost {
                self.workers.close_session_tool(instance.id);
            }
            self.write_end(
                ticket.task_id.clone(),
                instance,
                end.reason,
                Some(end.message),
            )
            .await;
        }
        result
    }

    /// Moves `ticket`'s call to the background under a new work id, writes `code_mode_detach`, and
    /// returns the result the model reads in its place. `None` when the call has already finished.
    pub(crate) async fn detach(&self, ticket: &CallTicket) -> Option<ToolResult> {
        let (work_id, after) = {
            let mut inner = self.lock();
            let running = inner
                .running
                .as_mut()
                .filter(|running| running.token == ticket.token)?;
            let work_id = crate::detached::new_work_id();
            let call = Arc::new(DetachedCall {
                started: running.started,
                outcome: watch::channel(None).0,
            });
            running.detached = Some(Arc::clone(&call));
            let after = running.started.elapsed();
            inner.detached.insert(work_id.clone(), call);
            (work_id, after)
        };
        if let Some(trace) = &self.trace {
            trace
                .write_code_mode_detach(
                    ticket.task_id.clone(),
                    &self.artifact,
                    &self.tool,
                    ticket.sequence,
                    &work_id,
                    u64::try_from(after.as_millis()).unwrap_or(u64::MAX),
                )
                .await;
        }
        Some(detach_result(&work_id))
    }

    /// Drops `ticket`'s call, still attached, whose awaiting dispatch went away: the instance it
    /// was cut in ends as `canceled`. A call that finished or detached is left alone.
    pub(crate) fn cancel(self: &Arc<Self>, ticket: &CallTicket) {
        let instance = {
            let mut inner = self.lock();
            let Some(running) = inner
                .running
                .take_if(|running| running.token == ticket.token && running.detached.is_none())
            else {
                return;
            };
            if let Some(abort) = &running.abort {
                abort.abort();
            }
            inner.instance.take()
        };
        let Some(instance) = instance else {
            return;
        };
        self.workers.close_session_tool(instance.id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let code_mode = Arc::clone(self);
            let task_id = ticket.task_id.clone();
            handle.spawn(async move {
                code_mode
                    .write_end(task_id, instance, EndReason::Canceled, None)
                    .await;
            });
        }
    }

    /// Whether `work_id` names a call this session moved to the background.
    pub(crate) fn knows(&self, work_id: &str) -> bool {
        self.lock().detached.contains_key(work_id)
    }

    fn call(&self, work_id: &str) -> Option<Arc<DetachedCall>> {
        self.lock().detached.get(work_id).cloned()
    }

    /// The detached call `work_id`'s state, or `None` for an id this session never issued.
    pub(crate) fn snapshot(&self, work_id: &str) -> Option<CallSnapshot> {
        let call = self.call(work_id)?;
        let outcome = call.outcome.borrow().clone();
        let (state, elapsed) = match outcome {
            None => (CallState::Running, call.started.elapsed()),
            Some(CallOutcome::Finished { result, elapsed }) => {
                (CallState::Finished(result), elapsed)
            }
            Some(CallOutcome::Stopped { elapsed }) => (CallState::Stopped, elapsed),
        };
        Some(CallSnapshot {
            work_id: work_id.to_string(),
            tool: self.tool.clone(),
            state,
            elapsed,
        })
    }

    /// Waits up to `bound` for the detached call `work_id` to finish or stop, or for `cancel`.
    pub(crate) async fn wait(
        &self,
        work_id: &str,
        bound: Duration,
        cancel: Option<&crate::cancel::CancelSignal>,
    ) -> WaitEnd {
        let Some(call) = self.call(work_id) else {
            return WaitEnd::Unknown;
        };
        let mut outcome = call.outcome.subscribe();
        let canceled = async {
            match cancel {
                Some(signal) => signal.canceled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            ended = outcome.wait_for(Option::is_some) => {
                if ended.is_ok() { WaitEnd::Finished } else { WaitEnd::TimedOut }
            }
            () = tokio::time::sleep(bound) => WaitEnd::TimedOut,
            () = canceled => WaitEnd::Canceled,
        }
    }

    /// Stops the detached call `work_id` by ending the instance it runs in, which writes
    /// `code_mode_instance_end` with `reason: stopped`. A call that already finished is sent
    /// nothing. `false` for an id this session never issued.
    pub(crate) async fn stop(&self, work_id: &str) -> bool {
        let (stopped, task_id) = {
            let mut inner = self.lock();
            let Some(call) = inner.detached.get(work_id).cloned() else {
                return false;
            };
            let Some(running) = inner.running.take_if(|running| {
                running
                    .detached
                    .as_ref()
                    .is_some_and(|running| Arc::ptr_eq(running, &call))
            }) else {
                return true;
            };
            if let Some(abort) = &running.abort {
                abort.abort();
            }
            call.outcome.send_replace(Some(CallOutcome::Stopped {
                elapsed: running.started.elapsed(),
            }));
            (inner.instance.take(), None)
        };
        if let Some(instance) = stopped {
            self.workers.close_session_tool(instance.id);
            self.write_end(task_id, instance, EndReason::Stopped, None)
                .await;
        }
        true
    }

    /// Ends the instance at the session's end, stopping a call still running in it, and writes
    /// `code_mode_instance_end` with `reason: session_end`. No instance opens after this.
    pub(crate) async fn end_session(&self) {
        let instance = {
            let mut inner = self.lock();
            inner.closed = true;
            if let Some(running) = inner.running.take() {
                if let Some(abort) = &running.abort {
                    abort.abort();
                }
                if let Some(call) = &running.detached {
                    call.outcome.send_replace(Some(CallOutcome::Stopped {
                        elapsed: running.started.elapsed(),
                    }));
                }
            }
            inner.instance.take()
        };
        if let Some(instance) = instance {
            self.workers.close_session_tool(instance.id);
            self.write_end(None, instance, EndReason::SessionEnd, None)
                .await;
        }
    }

    async fn write_end(
        &self,
        task_id: Option<String>,
        instance: Instance,
        reason: EndReason,
        message: Option<String>,
    ) {
        if let Some(trace) = &self.trace {
            trace
                .write_code_mode_instance_end(
                    task_id,
                    &self.artifact,
                    &self.tool,
                    instance.sequence,
                    reason.wire_name(),
                    instance.calls,
                    message.filter(|_| reason.carries_message()).as_deref(),
                )
                .await;
        }
    }
}

/// The work id `call` was registered under.
fn call_work_id(inner: &Inner, call: &Arc<DetachedCall>) -> String {
    inner
        .detached
        .iter()
        .find(|(_, held)| Arc::ptr_eq(held, call))
        .map(|(work_id, _)| work_id.clone())
        .unwrap_or_default()
}

/// [`CodeMode::offered_only`] for a tool named `tool`.
pub(crate) fn offered_only(tool: &str, tools_visible: bool, detaches: bool) -> Option<Vec<String>> {
    if tools_visible {
        return None;
    }
    let mut names = vec![tool.to_string()];
    if detaches {
        names.push(BACKGROUND_COMMAND_TOOL.to_string());
    }
    Some(names)
}

/// [`CodeMode::offered_only`] for a staged `code_mode` block.
pub(crate) fn offered_only_for(config: &CodeModeConfig) -> Option<Vec<String>> {
    offered_only(
        murmur_artifact::tool_name_of(&config.repl),
        config.tools_visible,
        config.detach_after_secs.is_some(),
    )
}

/// The result a call moved to the background returns in place of its own.
pub(crate) fn detach_result(work_id: &str) -> ToolResult {
    ToolResult {
        status: Status::Passed,
        summary: Some(format!("{work_id} running")),
        data: Some(format!(
            "The call is still running and was moved to the background as {work_id}. The \
             background-command tool reports on it, waits for it or stops it by that id. \
             Stopping it ends the code mode instance, and every value the instance holds is \
             lost. A call still running when the session ends is stopped."
        )),
        data_path: None,
        truncated: false,
        metadata: vec![
            ("work_id".to_string(), work_id.to_string()),
            ("state".to_string(), "running".to_string()),
        ],
        attachments: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_mode_offers_every_tool_unless_tools_visible_is_false() {
        assert_eq!(offered_only("counter-tool", true, true), None);
        assert_eq!(
            offered_only("counter-tool", false, false),
            Some(vec!["counter-tool".to_string()])
        );
        assert_eq!(
            offered_only("counter-tool", false, true),
            Some(vec![
                "counter-tool".to_string(),
                BACKGROUND_COMMAND_TOOL.to_string()
            ])
        );
    }

    #[test]
    fn code_mode_detach_result_names_the_work_id() {
        let result = detach_result("wrk_0a1b");
        assert!(matches!(result.status, Status::Passed));
        assert_eq!(result.summary.as_deref(), Some("wrk_0a1b running"));
        assert_eq!(
            result.data.as_deref(),
            Some(
                "The call is still running and was moved to the background as wrk_0a1b. The \
                 background-command tool reports on it, waits for it or stops it by that id. \
                 Stopping it ends the code mode instance, and every value the instance holds is \
                 lost. A call still running when the session ends is stopped."
            )
        );
        assert_eq!(
            result.metadata,
            vec![
                ("work_id".to_string(), "wrk_0a1b".to_string()),
                ("state".to_string(), "running".to_string()),
            ]
        );
    }

    #[test]
    fn code_mode_end_reasons_have_their_wire_names() {
        for (reason, name, message) in [
            (EndReason::SessionEnd, "session_end", false),
            (EndReason::Stopped, "stopped", false),
            (EndReason::Trap, "trap", true),
            (EndReason::Deadline, "deadline", true),
            (EndReason::ResourceLimit, "resource_limit", true),
            (EndReason::Canceled, "canceled", false),
            (EndReason::WorkerLost, "worker_lost", false),
        ] {
            assert_eq!(reason.wire_name(), name);
            assert_eq!(reason.carries_message(), message, "{name}");
        }
    }

    #[test]
    fn code_mode_failures_map_to_their_end_reasons() {
        let end = |kind| {
            InstanceEnd::from_failure(&ToolCallFailure {
                kind,
                message: "m".to_string(),
            })
            .reason
        };
        assert_eq!(end(ToolFailureKind::WallDeadline), EndReason::Deadline);
        assert_eq!(
            end(ToolFailureKind::Guest(GuestFailure::DeadlineExceeded {
                seconds: 2
            })),
            EndReason::Deadline
        );
        assert_eq!(
            end(ToolFailureKind::Guest(GuestFailure::ResourceLimit {
                message: "m".to_string()
            })),
            EndReason::ResourceLimit
        );
        assert_eq!(
            end(ToolFailureKind::Guest(GuestFailure::Other)),
            EndReason::Trap
        );
        assert_eq!(end(ToolFailureKind::Setup), EndReason::Trap);
    }

    #[test]
    fn code_mode_sentences_name_the_tool() {
        assert_eq!(
            instance_ended_sentence("counter-tool"),
            "The code mode instance ended; every value it held is lost, and the next call to \
             'counter-tool' starts a new instance."
        );
        assert_eq!(
            stopped_sentence("counter-tool"),
            "The call was stopped by ending the code mode instance; every value the instance \
             held is lost, and the next call to 'counter-tool' starts a new instance."
        );
    }
}
