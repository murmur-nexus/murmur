use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use murmur_artifact::TaskAcceptance;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::{cancel::CancelSignal, lanes::TaskLane, origin::TaskProvenance};

// ── JSON-RPC 2.0 envelope types ───────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JsonRpcRequest {
    #[allow(dead_code)] // parsed as part of the JSON-RPC 2.0 envelope; not validated
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub(crate) fn ok(id: Value, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub(crate) fn err(id: Value, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
            }),
        }
    }

    pub(crate) fn into_http_response(self) -> String {
        let body = serde_json::to_string(&self).unwrap_or_else(|_| "{}".to_string());
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body,
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

// ── A2A protocol types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2aMessage {
    pub message_id: String,
    pub context_id: Option<String>,
    #[allow(dead_code)] // part of the A2A Message schema; role validation deferred
    pub role: String,
    pub parts: Vec<MessagePart>,
}

impl A2aMessage {
    pub(crate) fn extract_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct MessagePart {
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct A2aTask {
    pub id: String,
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<A2aArtifact>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct A2aArtifact {
    pub name: String,
    pub parts: Vec<ArtifactPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtifactPart {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskStatus {
    pub state: TaskState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Rejected,
    /// A person stopped this task. Terminal, and distinct from `Failed`: nothing went wrong, the
    /// work was called off. Spelled `"canceled"` on the wire, which is the A2A protocol's own
    /// spelling.
    Canceled,
}

impl TaskState {
    /// Whether no further work will be done on a task in this state.
    ///
    /// The one rule a cancel reads: a terminal task is left exactly as it is, and only a live one
    /// can be stopped.
    pub(crate) fn is_terminal(&self) -> bool {
        match self {
            Self::Submitted | Self::Working | Self::InputRequired => false,
            Self::Completed | Self::Failed | Self::Rejected | Self::Canceled => true,
        }
    }
}

/// What [`TaskRegistry::request_cancel`] did.
///
/// Three outcomes and only one of them is an error at the door: cancelling a task that has
/// already ended is a clean no-op, because the caller's intent — "do no more work on this" — is
/// already true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelOutcome {
    /// The task was live and is now `Canceled`. Its [`CancelSignal`] has been raised.
    Accepted,
    /// The task had already reached a terminal state, which is left untouched.
    AlreadyTerminal,
    /// No task with this id was ever enqueued here.
    Unknown,
}

// ── Task slot state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) enum TaskSlotState {
    Empty,
    Running {
        task_id: String,
        context_id: String,
        /// The lane the queue chose this task out of. Set and cleared with the rest of the
        /// variant, so it cannot name a lane no task is running in.
        lane: TaskLane,
    },
    Done {
        task_id: String,
    },
}

// ── TaskRegistry — multi-task history tracker ─────────────────────────────────

/// Replaces the bare `Arc<Mutex<TaskSlotState>>` at the serve_http boundary.
/// Tracks all tasks (active + historical) for queue-mode capsules.
pub(crate) struct TaskRegistry {
    pub(crate) active_slot: TaskSlotState,
    /// All tasks ever enqueued: task_id → (state, context_id)
    pub(crate) history: HashMap<String, (TaskState, String)>,
    pub(crate) pending_count: usize,
    pub(crate) queue_depth: usize,
    pub(crate) task_acceptance: TaskAcceptance,
    /// Pending input waiters: task_id → (prompt, oneshot sender)
    input_waiters: HashMap<String, (String, oneshot::Sender<String>)>,
    /// One signal per task anybody has asked about, whether or not it has been cancelled.
    ///
    /// Minted on demand rather than at enqueue, because both ends need one before the task is
    /// running: the task loop watches a task it is about to activate, and the door cancels one
    /// that is still queued.
    cancels: HashMap<String, CancelSignal>,
    /// Which completed turn the capsule's exported files are as of, shared with the resource
    /// plane. Lives here because every terminal state passes through this registry, so a third
    /// [`Self::finish_task`] call site cannot appear with no matching increment beside it.
    resource_generation: Arc<AtomicU64>,
}

impl TaskRegistry {
    pub(crate) fn new(queue_depth: usize, task_acceptance: TaskAcceptance) -> Self {
        Self {
            active_slot: TaskSlotState::Empty,
            history: HashMap::new(),
            pending_count: 0,
            queue_depth,
            task_acceptance,
            input_waiters: HashMap::new(),
            cancels: HashMap::new(),
            resource_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A handle on the generation counter, for a reader that must not take this registry's lock.
    ///
    /// The resource plane serves reads while a turn is running, so it holds its own `Arc` and
    /// loads it atomically. Taking the registry mutex to answer a `GET` would make a read wait on
    /// the agent loop, which is the one thing this plane promises never to do.
    pub(crate) fn resource_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.resource_generation)
    }

    /// Advance the generation by one. Called at each [`Self::finish_task`] call site, immediately
    /// after the task reaches a terminal state.
    ///
    /// Provenance, not a pin: it answers "these bytes are as of turn N" and nothing else. No
    /// request selects a generation, no response is refused because it moved, and no superseded
    /// bytes are retained.
    pub(crate) fn advance_resource_generation(&self) {
        self.resource_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn can_accept(&self) -> bool {
        match self.task_acceptance {
            TaskAcceptance::None => false,
            TaskAcceptance::Single => {
                matches!(self.active_slot, TaskSlotState::Empty) && self.pending_count == 0
            }
            TaskAcceptance::Queue => self.pending_count < self.queue_depth,
        }
    }

    pub(crate) fn enqueue(&mut self, task_id: &str, context_id: &str) {
        self.pending_count += 1;
        self.history.insert(
            task_id.to_string(),
            (TaskState::Submitted, context_id.to_string()),
        );
    }

    /// `lane` is the lane the queue selected this task out of, so what the registry reports is
    /// what the selection did.
    pub(crate) fn start_task(&mut self, task_id: String, context_id: String, lane: TaskLane) {
        debug_assert!(self.pending_count > 0);
        self.pending_count -= 1;
        self.history
            .insert(task_id.clone(), (TaskState::Working, context_id.clone()));
        self.active_slot = TaskSlotState::Running {
            task_id,
            context_id,
            lane,
        };
    }

    /// The lane of the task the capsule is running, or `None` when no task is running.
    ///
    /// `Some` is what stops the lane queue yielding anything, so a `Done` or `Empty` slot must
    /// answer `None`.
    pub(crate) fn active_lane(&self) -> Option<TaskLane> {
        match self.active_slot {
            TaskSlotState::Running { lane, .. } => Some(lane),
            TaskSlotState::Empty | TaskSlotState::Done { .. } => None,
        }
    }

    pub(crate) fn finish_task(&mut self, final_state: TaskState) {
        if let TaskSlotState::Running {
            ref task_id,
            ref context_id,
            ..
        } = self.active_slot
        {
            let (tid, cid) = (task_id.clone(), context_id.clone());
            self.input_waiters.remove(&tid);
            self.cancels.remove(&tid);
            // An accepted cancel is final. A turn a person stopped must never later read as one
            // that ran to completion, so the outcome the loop reports loses to the one already
            // recorded.
            let final_state = match self.history.get(&tid) {
                Some((TaskState::Canceled, _)) => TaskState::Canceled,
                _ => final_state,
            };
            self.history.insert(tid.clone(), (final_state, cid));
            self.active_slot = TaskSlotState::Done { task_id: tid };
        }
    }

    /// Transition the active task to InputRequired, storing the prompt and the
    /// oneshot sender that will deliver the external response.
    pub(crate) fn set_input_required(
        &mut self,
        task_id: &str,
        prompt: String,
        tx: oneshot::Sender<String>,
    ) -> Result<(), &'static str> {
        match &self.active_slot {
            TaskSlotState::Running {
                task_id: active_id, ..
            } if active_id == task_id => {}
            _ => return Err("task is not the active running task"),
        }
        let state = self.history.get(task_id).map(|(s, _)| s);
        if !matches!(state, Some(TaskState::Working)) {
            return Err("task is not in working state");
        }
        if let Some((_, ctx)) = self.history.get(task_id).cloned() {
            self.history
                .insert(task_id.to_string(), (TaskState::InputRequired, ctx));
        }
        self.input_waiters.insert(task_id.to_string(), (prompt, tx));
        Ok(())
    }

    /// Deliver external input to an input-required task, transitioning it back to Working.
    pub(crate) fn deliver_input(
        &mut self,
        task_id: &str,
        text: String,
    ) -> Result<(), &'static str> {
        let state = self.history.get(task_id).map(|(s, _)| s);
        if !matches!(state, Some(TaskState::InputRequired)) {
            return Err("task is not in input-required state");
        }
        let Some((_, tx)) = self.input_waiters.remove(task_id) else {
            return Err("no input waiter found for task");
        };
        if let Some((_, ctx)) = self.history.get(task_id).cloned() {
            self.history
                .insert(task_id.to_string(), (TaskState::Working, ctx));
        }
        // Sending may fail if the receiver was dropped (timeout path), which is fine.
        let _ = tx.send(text);
        Ok(())
    }

    /// Return the task_id of the active task if it is currently in InputRequired state.
    pub(crate) fn active_input_required_task_id(&self) -> Option<String> {
        if let TaskSlotState::Running { ref task_id, .. } = self.active_slot {
            let state = self.history.get(task_id).map(|(s, _)| s);
            if matches!(state, Some(TaskState::InputRequired)) {
                return Some(task_id.clone());
            }
        }
        None
    }

    /// The cancel signal for `task_id`, minting one if nobody has asked yet.
    ///
    /// Handed to whatever has to race against a cancel — the driver call, the input wait, the
    /// delegation wait — and to the door, which raises it. A signal for a task that never runs
    /// costs one entry and is dropped with the task's terminal state.
    pub(crate) fn cancel_watch(&mut self, task_id: &str) -> CancelSignal {
        self.cancels.entry(task_id.to_string()).or_default().clone()
    }

    /// Whether this task's recorded state is `Canceled`.
    ///
    /// Read at the task loop's activation step, which is what keeps a task cancelled while it was
    /// still `submitted` from ever starting.
    pub(crate) fn is_canceled(&self, task_id: &str) -> bool {
        matches!(self.history.get(task_id), Some((TaskState::Canceled, _)))
    }

    /// Stop one task: record `Canceled` and raise its signal.
    ///
    /// Called on the door's connection task and never waits for the agent loop to acknowledge —
    /// the state is written here, so a `tasks/get` that lands next already reads `canceled`
    /// whatever the loop is in the middle of.
    ///
    /// A cancelled task that was still `submitted` gives its queue slot back immediately: it will
    /// be skipped when the loop reaches it, so holding capacity for it would refuse work the
    /// capsule can do.
    pub(crate) fn request_cancel(&mut self, task_id: &str) -> CancelOutcome {
        let Some((state, context_id)) = self.history.get(task_id).cloned() else {
            return CancelOutcome::Unknown;
        };
        if state.is_terminal() {
            return CancelOutcome::AlreadyTerminal;
        }
        if matches!(state, TaskState::Submitted) {
            self.pending_count = self.pending_count.saturating_sub(1);
        }
        self.history
            .insert(task_id.to_string(), (TaskState::Canceled, context_id));
        self.cancel_watch(task_id).cancel();
        CancelOutcome::Accepted
    }

    /// Return the prompt stored for an input-required task.
    #[allow(dead_code)] // used in unit tests
    pub(crate) fn get_input_prompt(&self, task_id: &str) -> Option<&str> {
        self.input_waiters
            .get(task_id)
            .map(|(prompt, _)| prompt.as_str())
    }

    pub(crate) fn get_task(&self, task_id: &str) -> Option<A2aTask> {
        self.history.get(task_id).map(|(state, context_id)| {
            let artifacts = if matches!(state, TaskState::InputRequired) {
                self.input_waiters.get(task_id).map(|(prompt, _)| {
                    vec![A2aArtifact {
                        name: "prompt".to_string(),
                        parts: vec![ArtifactPart {
                            text: prompt.clone(),
                        }],
                    }]
                })
            } else {
                None
            };
            A2aTask {
                id: task_id.to_string(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: state.clone(),
                },
                artifacts,
            }
        })
    }
}

// ── Incoming task (HTTP server → main thread) ─────────────────────────────────

#[derive(Debug)]
pub(crate) struct IncomingTask {
    pub task_id: String,
    pub context_id: String,
    pub message_id: String,
    pub message_text: String,
    pub traceparent: Option<String>,
    /// Why this task woke the capsule, classified at the door from the request headers. Every
    /// inbound task has one: a caller that claims nothing is an untrusted `event`.
    pub provenance: TaskProvenance,
    /// Where the task came from, as it appears on its `task_start` record: `"a2a"` for anything
    /// that arrived over the peer door, `"detached_shell"` for a completion the runtime produced
    /// locally. The `a2a_task_received` record is written only for the former.
    pub source: &'static str,
    /// The delegation this task reports on: one the door classified `completion` and carrying
    /// [`crate::delegation::DELEGATION_ID_HEADER`], posted by the sub-capsule itself or by the
    /// completion watcher behind it. `None` for every other task, including a locally produced
    /// detached-shell completion, which reports on a work id rather than a delegation.
    pub delegation_id: Option<String>,
}

/// The `source` of a task that arrived over the A2A door.
pub(crate) const SOURCE_A2A: &str = "a2a";

/// The `source` of a task the runtime enqueued for itself when a demoted shell command finished.
pub(crate) const SOURCE_DETACHED_SHELL: &str = "detached_shell";

/// The `source` of a task a resumed launch enqueued for itself to report demoted commands the
/// session it resumes never accounted for. Distinct from [`SOURCE_DETACHED_SHELL`] because the
/// two say opposite things: one carries a result, the other says no result exists.
pub(crate) const SOURCE_DETACHED_LOST: &str = "detached_lost";

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> TaskRegistry {
        TaskRegistry::new(1, TaskAcceptance::Single)
    }

    fn running_registry(task_id: &str) -> TaskRegistry {
        let mut r = make_registry();
        r.enqueue(task_id, "ctx_001");
        r.start_task(task_id.to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r
    }

    #[test]
    fn set_input_required_transitions_state() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        assert!(r
            .set_input_required("tsk_001", "which branch?".into(), tx)
            .is_ok());
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::InputRequired);
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts[0].name, "prompt");
        assert_eq!(artifacts[0].parts[0].text, "which branch?");
    }

    #[test]
    fn set_input_required_fails_for_wrong_task() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        assert!(r
            .set_input_required("tsk_other", "prompt".into(), tx)
            .is_err());
    }

    #[test]
    fn deliver_input_transitions_back_to_working() {
        let mut r = running_registry("tsk_001");
        let (tx, mut rx) = oneshot::channel();
        r.set_input_required("tsk_001", "which branch?".into(), tx)
            .unwrap();
        r.deliver_input("tsk_001", "main".to_string()).unwrap();
        // Oneshot should have received the value
        assert_eq!(rx.try_recv().unwrap(), "main");
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::Working);
        assert!(task.artifacts.is_none());
    }

    #[test]
    fn deliver_input_fails_when_not_input_required() {
        let mut r = running_registry("tsk_001");
        assert!(r.deliver_input("tsk_001", "text".into()).is_err());
    }

    #[test]
    fn active_input_required_task_id_returns_correct() {
        let mut r = running_registry("tsk_001");
        assert_eq!(r.active_input_required_task_id(), None);
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "prompt".into(), tx)
            .unwrap();
        assert_eq!(
            r.active_input_required_task_id(),
            Some("tsk_001".to_string())
        );
        r.deliver_input("tsk_001", "answer".into()).unwrap();
        assert_eq!(r.active_input_required_task_id(), None);
    }

    #[test]
    fn get_task_includes_artifacts_only_for_input_required() {
        let mut r = running_registry("tsk_001");
        let task = r.get_task("tsk_001").unwrap();
        assert!(
            task.artifacts.is_none(),
            "working task should have no artifacts"
        );

        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "my prompt".into(), tx)
            .unwrap();
        let task = r.get_task("tsk_001").unwrap();
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].parts[0].text, "my prompt");
    }

    #[test]
    fn request_cancel_on_a_running_task_records_canceled() {
        let mut r = running_registry("tsk_001");
        let signal = r.cancel_watch("tsk_001");
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::Accepted);
        assert!(signal.is_canceled());
        assert!(r.is_canceled("tsk_001"));
        let task = r.get_task("tsk_001").unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
    }

    #[test]
    fn request_cancel_on_a_completed_task_leaves_it_alone() {
        let mut r = running_registry("tsk_001");
        r.finish_task(TaskState::Completed);
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::AlreadyTerminal);
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Completed
        );
        // And a second cancel of an already-cancelled task says the same thing.
        let mut r = running_registry("tsk_002");
        assert_eq!(r.request_cancel("tsk_002"), CancelOutcome::Accepted);
        assert_eq!(r.request_cancel("tsk_002"), CancelOutcome::AlreadyTerminal);
    }

    #[test]
    fn request_cancel_on_an_unknown_id_is_unknown() {
        let mut r = make_registry();
        assert_eq!(r.request_cancel("tsk_doesnotexist"), CancelOutcome::Unknown);
        assert!(!r.is_canceled("tsk_doesnotexist"));
    }

    #[test]
    fn finish_task_does_not_overwrite_an_accepted_cancel() {
        let mut r = running_registry("tsk_001");
        assert_eq!(r.request_cancel("tsk_001"), CancelOutcome::Accepted);
        r.finish_task(TaskState::Completed);
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
        r.finish_task(TaskState::Failed);
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
    }

    #[test]
    fn cancelling_a_queued_task_frees_its_queue_slot() {
        let mut r = TaskRegistry::new(2, TaskAcceptance::Queue);
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_b", "ctx_001");
        assert!(!r.can_accept(), "the queue is full");
        assert_eq!(r.request_cancel("tsk_b"), CancelOutcome::Accepted);
        assert!(r.can_accept(), "a cancelled task holds no slot");
    }

    #[test]
    fn canceled_serializes_with_the_protocol_spelling() {
        let json = serde_json::to_string(&TaskState::Canceled).unwrap();
        assert_eq!(json, "\"canceled\"");
        // Every existing spelling is unchanged by the new variant.
        assert_eq!(
            serde_json::to_string(&TaskState::InputRequired).unwrap(),
            "\"input-required\""
        );
    }

    #[test]
    fn finish_task_cleans_up_input_waiters() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "prompt".into(), tx)
            .unwrap();
        r.finish_task(TaskState::Failed);
        // input_waiters should be cleaned up
        assert!(r.get_input_prompt("tsk_001").is_none());
    }
}
