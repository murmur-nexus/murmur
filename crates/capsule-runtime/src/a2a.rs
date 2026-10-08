use std::{
    collections::{HashMap, HashSet},
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
    /// `{"murmur": {...}}` for a terminal task with something to say about answers it lacks:
    /// `noAnswer: true` for one ended through `end-without-answer`, and `noAnswerBelow`, the
    /// members further down that gave it none. Absent when it has neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct A2aArtifact {
    pub name: String,
    pub parts: Vec<ArtifactPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ArtifactPart {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TaskStatus {
    pub state: TaskState,
    /// What the task's final status said, for a terminal task that said something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<StatusMessage>,
}

impl TaskStatus {
    /// A status in `state` with no message.
    pub(crate) fn of(state: TaskState) -> Self {
        Self {
            state,
            message: None,
        }
    }
}

/// A task status's message: an A2A message from the agent with one text part, in the shape this
/// door reads an incoming message in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusMessage {
    pub message_id: String,
    pub role: String,
    pub parts: Vec<ArtifactPart>,
}

impl StatusMessage {
    /// The agent's message `text` about `task_id`.
    pub(crate) fn agent(task_id: &str, text: &str) -> Self {
        Self {
            message_id: format!("msg_{task_id}_status"),
            role: "agent".to_string(),
            parts: vec![ArtifactPart {
                text: text.to_string(),
            }],
        }
    }
}

/// How a finished task ended, as its final status reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskEnding {
    /// The final status's message.
    message: String,
    /// The task's response text, kept only for a `completed` task.
    response: Option<String>,
    /// Whether the task ended through `end-without-answer`.
    no_answer: bool,
    /// `(member, status)` for each member further down that gave the task no answer.
    no_answer_below: Vec<(String, String)>,
}

impl TaskEnding {
    /// The `metadata` `tasks/get` carries for this ending, or `None` when it says nothing.
    fn metadata(&self) -> Option<serde_json::Value> {
        let mut murmur = serde_json::Map::new();
        if self.no_answer {
            murmur.insert("noAnswer".to_string(), serde_json::Value::Bool(true));
        }
        if !self.no_answer_below.is_empty() {
            let below = self
                .no_answer_below
                .iter()
                .map(|(member, status)| serde_json::json!({"member": member, "status": status}))
                .collect();
            murmur.insert("noAnswerBelow".to_string(), serde_json::Value::Array(below));
        }
        (!murmur.is_empty()).then(|| serde_json::json!({ "murmur": murmur }))
    }
}

/// The name of the artifact a completed task's response is carried in over `tasks/get`.
pub(crate) const RESPONSE_ARTIFACT: &str = "response";

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
    /// The state's wire spelling, the one `tasks/get` serializes and a stream frame's
    /// `status.state` carries.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Working => "working",
            Self::InputRequired => "input-required",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Canceled => "canceled",
        }
    }

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
    /// Tasks whose `request-input` wait timed out, until the agent loop takes the mark.
    input_timeouts: HashSet<String>,
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
    /// Set by [`Self::close_to_new_work`] once the task loop has ended. A closed registry
    /// accepts nothing, whatever its acceptance mode and depth.
    closed: bool,
    /// How each finished task ended, recorded by [`Self::record_ending`].
    endings: HashMap<String, TaskEnding>,
    /// The formation member that submitted each task the door let in on a formation token.
    submitters: HashMap<String, String>,
}

/// The `status.message` of a task refused because the session ended before it started.
pub(crate) const REJECTED_SESSION_ENDED_MESSAGE: &str =
    "task rejected: the session ended before this task started";

/// The `status.message` of a task refused because `mur stop` ended the session before it started.
pub(crate) const REJECTED_SESSION_STOPPED_MESSAGE: &str =
    "task rejected: the session was stopped before this task started";

/// The `status.message` the door's `message/stream` refusal carries once the registry is closed.
pub(crate) const REJECTED_SESSION_CLOSING_MESSAGE: &str = "task rejected: the session is closing";

/// The `status.message` of a task refused because the capsule has no room for it. A `call-member`
/// call reads exactly this text as "busy" and offers the task again; every other refusal ends it.
pub(crate) const REJECTED_BUSY_MESSAGE: &str = "task rejected: capsule is busy";

impl TaskRegistry {
    pub(crate) fn new(queue_depth: usize, task_acceptance: TaskAcceptance) -> Self {
        Self {
            active_slot: TaskSlotState::Empty,
            history: HashMap::new(),
            pending_count: 0,
            queue_depth,
            task_acceptance,
            input_waiters: HashMap::new(),
            input_timeouts: HashSet::new(),
            cancels: HashMap::new(),
            resource_generation: Arc::new(AtomicU64::new(0)),
            closed: false,
            endings: HashMap::new(),
            submitters: HashMap::new(),
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
        if self.closed {
            return false;
        }
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

    /// Record that the formation member `member` submitted `task_id`. Called under the lock the
    /// task was enqueued under, so no `tasks/get` can see the task without its submitter.
    pub(crate) fn record_submitter(&mut self, task_id: &str, member: &str) {
        self.submitters
            .insert(task_id.to_string(), member.to_string());
    }

    /// The formation member that submitted `task_id`, or `None` for a task no formation token
    /// submitted.
    pub(crate) fn submitter(&self, task_id: &str) -> Option<&str> {
        self.submitters.get(task_id).map(String::as_str)
    }

    /// Record how `task_id` ended: its final status's `message`, and its `response` when it
    /// completed. A response on any other state is not kept. `no_answer` is whether it ended
    /// through `end-without-answer`, and `no_answer_below` the `(member, status)` of each member
    /// further down that gave it no answer.
    pub(crate) fn record_ending(
        &mut self,
        task_id: &str,
        message: &str,
        response: Option<&str>,
        no_answer: bool,
        no_answer_below: &[(String, String)],
    ) {
        let completed = matches!(self.history.get(task_id), Some((TaskState::Completed, _)));
        self.endings.insert(
            task_id.to_string(),
            TaskEnding {
                message: message.to_string(),
                response: response.filter(|_| completed).map(str::to_string),
                no_answer,
                no_answer_below: no_answer_below.to_vec(),
            },
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

    /// End the running task in `final_state`, and return the state actually recorded: an
    /// accepted cancel already in the history wins over whatever is passed. `None` when no task
    /// is running, which records nothing.
    pub(crate) fn finish_task(&mut self, final_state: TaskState) -> Option<TaskState> {
        let TaskSlotState::Running {
            ref task_id,
            ref context_id,
            ..
        } = self.active_slot
        else {
            return None;
        };
        let (tid, cid) = (task_id.clone(), context_id.clone());
        self.input_waiters.remove(&tid);
        self.input_timeouts.remove(&tid);
        self.cancels.remove(&tid);
        // An accepted cancel is final. A turn a person stopped must never later read as one
        // that ran to completion, so the outcome the loop reports loses to the one already
        // recorded.
        let final_state = match self.history.get(&tid) {
            Some((TaskState::Canceled, _)) => TaskState::Canceled,
            _ => final_state,
        };
        self.history.insert(tid.clone(), (final_state.clone(), cid));
        self.active_slot = TaskSlotState::Done { task_id: tid };
        Some(final_state)
    }

    /// Record that `task_id`'s `request-input` wait passed `lifecycle.input_timeout_secs`
    /// without ending the task: the waiter is dropped, an `input-required` task goes back to
    /// `working`, and the timeout is marked for [`Self::take_input_timeout`]. The task's
    /// terminal state is the reopen loop's to record, once its `on-task-end` hooks have run.
    pub(crate) fn record_input_timeout(&mut self, task_id: &str) {
        self.input_waiters.remove(task_id);
        if let Some((TaskState::InputRequired, ctx)) = self.history.get(task_id).cloned() {
            self.history
                .insert(task_id.to_string(), (TaskState::Working, ctx));
        }
        self.input_timeouts.insert(task_id.to_string());
    }

    /// Whether `task_id` has an unread input-wait timeout, clearing the mark. Read by the agent
    /// loop at each turn boundary, so the attempt that timed out ends and a reopened attempt of
    /// the same task does not end on the same mark.
    pub(crate) fn take_input_timeout(&mut self, task_id: &str) -> bool {
        self.input_timeouts.remove(task_id)
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

    /// Cancel every task that is not yet terminal, and return the ids that were cancelled, sorted.
    ///
    /// Each goes through [`Self::request_cancel`], so a running task's signal is raised and a
    /// `submitted` task's queue slot is given back. A second call returns nothing: every task the
    /// first one touched is `Canceled`, which is terminal.
    pub(crate) fn cancel_every_live(&mut self) -> Vec<String> {
        let live: Vec<String> = self
            .history
            .iter()
            .filter(|(_, (state, _))| !state.is_terminal())
            .map(|(task_id, _)| task_id.clone())
            .collect();
        let mut canceled: Vec<String> = live
            .into_iter()
            .filter(|task_id| matches!(self.request_cancel(task_id), CancelOutcome::Accepted))
            .collect();
        // `history` is a `HashMap`, so without this the same two tasks come back in a different
        // order on every call and nothing can diff two stops.
        canceled.sort();
        canceled
    }

    /// Stop taking work, and end every task still `submitted` in `Rejected`. Returns the refused
    /// tasks' `(task_id, context_id)`, sorted by task id, which for UUIDv7 ids is acceptance order.
    ///
    /// Taken under the same lock the door enqueues under, so no task can be enqueued after the
    /// call returns: [`Self::can_accept`] answers `false` from here on. Only `Submitted` tasks are
    /// touched. A `Canceled` task keeps its state, and a task that ran keeps the state it ended in.
    /// No turn ran, so the resource generation does not advance. A second call returns nothing.
    pub(crate) fn close_to_new_work(&mut self) -> Vec<(String, String)> {
        self.closed = true;
        let mut refused: Vec<(String, String)> = self
            .history
            .iter()
            .filter(|(_, (state, _))| matches!(state, TaskState::Submitted))
            .map(|(task_id, (_, context_id))| (task_id.clone(), context_id.clone()))
            .collect();
        refused.sort();
        for (task_id, context_id) in &refused {
            self.history
                .insert(task_id.clone(), (TaskState::Rejected, context_id.clone()));
            self.cancels.remove(task_id);
        }
        self.pending_count = self.pending_count.saturating_sub(refused.len());
        refused
    }

    /// Whether [`Self::close_to_new_work`] has run. The door reads it to tell a closing session
    /// from a busy one when it refuses a task.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Return the prompt stored for an input-required task.
    #[allow(dead_code)] // used in unit tests
    pub(crate) fn get_input_prompt(&self, task_id: &str) -> Option<&str> {
        self.input_waiters
            .get(task_id)
            .map(|(prompt, _)| prompt.as_str())
    }

    /// `task_id` as `tasks/get` reports it.
    ///
    /// An `input-required` task carries its prompt as a `prompt` artifact. A terminal task carries
    /// its final status's message as `status.message`, and a `completed` one its response as a
    /// `response` artifact, each when it has one.
    pub(crate) fn get_task(&self, task_id: &str) -> Option<A2aTask> {
        self.history.get(task_id).map(|(state, context_id)| {
            let artifact = |name: &str, text: &str| A2aArtifact {
                name: name.to_string(),
                parts: vec![ArtifactPart {
                    text: text.to_string(),
                }],
            };
            let ending = self.endings.get(task_id).filter(|_| state.is_terminal());
            let artifacts = match state {
                TaskState::InputRequired => self
                    .input_waiters
                    .get(task_id)
                    .map(|(prompt, _)| vec![artifact("prompt", prompt)]),
                TaskState::Completed => ending
                    .and_then(|ending| ending.response.as_deref())
                    .filter(|response| !response.is_empty())
                    .map(|response| vec![artifact(RESPONSE_ARTIFACT, response)]),
                _ => None,
            };
            let message = ending
                .filter(|ending| !ending.message.is_empty())
                .map(|ending| StatusMessage::agent(task_id, &ending.message));
            A2aTask {
                id: task_id.to_string(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: state.clone(),
                    message,
                },
                artifacts,
                metadata: ending.and_then(TaskEnding::metadata),
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
    /// Whether this request asked the capsule to drop the harness session its context names
    /// before the turn, under [`crate::identity::FORGET_SESSION_HEADER`]. `false` for every task
    /// that did not carry the header, and for every task the runtime enqueued for itself.
    pub forget_session: bool,
    /// The formation member that called, when the door let this task in on a formation token.
    /// Recorded as `a2a_task_received.caller_member`; `None` for every other task.
    pub caller_member: Option<String>,
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
    fn an_input_required_task_carries_its_prompt_and_nothing_else() {
        let mut r = running_registry("tsk_1");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_1", "Which file?".to_string(), tx)
            .unwrap();
        let task = r.get_task("tsk_1").unwrap();
        let artifacts = task.artifacts.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name, "prompt");
        assert_eq!(artifacts[0].parts[0].text, "Which file?");
        assert!(task.status.message.is_none());
    }

    /// A live task carries nothing beyond its state: no artifact and no status message.
    #[test]
    fn a_working_task_carries_no_artifact_and_no_message() {
        let r = running_registry("tsk_1");
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(
            task,
            serde_json::json!({"id": "tsk_1", "contextId": "ctx_001", "status": {"state": "working"}})
        );
    }

    /// A completed task answers with its response as the `response` artifact and its final
    /// message as `status.message`, an A2A message from the agent.
    #[test]
    fn a_completed_task_carries_its_response_artifact_and_status_message() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_1", "done", Some("the answer is 4"), false, &[]);
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(
            task,
            serde_json::json!({
                "id": "tsk_1",
                "contextId": "ctx_001",
                "status": {
                    "state": "completed",
                    "message": {
                        "messageId": "msg_tsk_1_status",
                        "role": "agent",
                        "parts": [{"text": "done"}],
                    },
                },
                "artifacts": [{"name": "response", "parts": [{"text": "the answer is 4"}]}],
            })
        );
    }

    /// A task ended through `end-without-answer` carries `metadata.murmur.noAnswer` and the
    /// members further down that gave it none; a completed task with members missing below it
    /// carries its response and `noAnswerBelow` but no `noAnswer`; any other ending carries no
    /// `metadata` at all.
    #[test]
    fn a_no_answer_ending_carries_its_metadata_and_no_other_ending_does() {
        let below = [("q".to_string(), "timed_out".to_string())];
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Failed);
        r.record_ending("tsk_1", "q gave no answer", None, true, &below);
        let task = serde_json::to_value(r.get_task("tsk_1").unwrap()).unwrap();
        assert_eq!(task["status"]["state"], "failed");
        assert_eq!(
            task["status"]["message"]["parts"][0]["text"],
            "q gave no answer"
        );
        assert_eq!(
            task["metadata"],
            serde_json::json!({"murmur": {
                "noAnswer": true,
                "noAnswerBelow": [{"member": "q", "status": "timed_out"}],
            }})
        );

        let mut r = running_registry("tsk_2");
        r.finish_task(TaskState::Completed);
        r.record_ending("tsk_2", "session ended", Some("none came"), false, &below);
        let task = serde_json::to_value(r.get_task("tsk_2").unwrap()).unwrap();
        assert_eq!(
            task["artifacts"],
            serde_json::json!([{"name": "response", "parts": [{"text": "none came"}]}])
        );
        assert_eq!(
            task["metadata"],
            serde_json::json!({"murmur": {
                "noAnswerBelow": [{"member": "q", "status": "timed_out"}],
            }})
        );

        for state in [TaskState::Completed, TaskState::Failed] {
            let mut r = running_registry("tsk_3");
            r.finish_task(state);
            r.record_ending("tsk_3", "ended", Some("4"), false, &[]);
            let task = serde_json::to_value(r.get_task("tsk_3").unwrap()).unwrap();
            assert!(task.get("metadata").is_none(), "{task}");
        }
        let r = running_registry("tsk_4");
        let task = serde_json::to_value(r.get_task("tsk_4").unwrap()).unwrap();
        assert!(task.get("metadata").is_none(), "{task}");
    }

    /// A task that did not complete carries its message and no response, whatever was passed.
    #[test]
    fn a_failed_task_carries_its_status_message_and_no_response() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Failed);
        r.record_ending("tsk_1", "the driver failed", Some("partial"), false, &[]);
        let task = r.get_task("tsk_1").unwrap();
        assert!(task.artifacts.is_none());
        let message = task.status.message.unwrap();
        assert_eq!(message.role, "agent");
        assert_eq!(message.parts[0].text, "the driver failed");
    }

    /// A completed task with no response text, or no recorded ending, carries no artifact.
    #[test]
    fn a_completed_task_without_a_response_carries_no_artifact() {
        let mut r = running_registry("tsk_1");
        r.finish_task(TaskState::Completed);
        assert!(r.get_task("tsk_1").unwrap().artifacts.is_none());
        r.record_ending("tsk_1", "ok", Some(""), false, &[]);
        let task = r.get_task("tsk_1").unwrap();
        assert!(task.artifacts.is_none());
        assert_eq!(task.status.message.unwrap().parts[0].text, "ok");
    }

    #[test]
    fn a_submitter_is_recorded_per_task() {
        let mut r = make_registry();
        r.enqueue("tsk_a", "ctx_001");
        r.record_submitter("tsk_a", "lead");
        r.enqueue("tsk_b", "ctx_001");
        assert_eq!(r.submitter("tsk_a"), Some("lead"));
        assert_eq!(r.submitter("tsk_b"), None);
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
    fn cancel_every_live_cancels_live_tasks_in_order_and_leaves_terminal_ones() {
        let mut r = TaskRegistry::new(8, TaskAcceptance::Queue);
        r.enqueue("tsk_c", "ctx_001");
        r.enqueue("tsk_a", "ctx_001");
        r.enqueue("tsk_done", "ctx_001");
        r.start_task("tsk_done".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        r.finish_task(TaskState::Completed);
        r.start_task("tsk_a".to_string(), "ctx_001".to_string(), TaskLane::Bg);
        let running = r.cancel_watch("tsk_a");

        assert_eq!(r.cancel_every_live(), vec!["tsk_a", "tsk_c"]);
        assert!(running.is_canceled());
        assert!(r.is_canceled("tsk_c"));
        assert_eq!(
            r.get_task("tsk_done").unwrap().status.state,
            TaskState::Completed
        );
        assert!(r.cancel_every_live().is_empty());
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
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Canceled),
            "the state recorded is the accepted cancel, and the caller is told so"
        );
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(
            r.finish_task(TaskState::Failed),
            None,
            "a slot that is no longer running records nothing"
        );
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Canceled
        );
    }

    #[test]
    fn finish_task_returns_the_state_it_recorded() {
        let mut r = running_registry("tsk_001");
        assert_eq!(
            r.finish_task(TaskState::Completed),
            Some(TaskState::Completed)
        );
        assert_eq!(make_registry().finish_task(TaskState::Failed), None);
    }

    #[test]
    fn an_input_timeout_leaves_the_task_working_and_is_read_once() {
        let mut r = running_registry("tsk_001");
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_001", "prompt".into(), tx)
            .unwrap();
        r.record_input_timeout("tsk_001");
        assert_eq!(
            r.get_task("tsk_001").unwrap().status.state,
            TaskState::Working,
            "the task's terminal state is not the wait's to record"
        );
        assert!(
            r.get_input_prompt("tsk_001").is_none(),
            "the waiter is gone"
        );
        assert_eq!(r.active_input_required_task_id(), None);
        assert!(r.take_input_timeout("tsk_001"));
        assert!(
            !r.take_input_timeout("tsk_001"),
            "a reopened attempt does not end on the same timeout"
        );
        assert!(!r.take_input_timeout("tsk_other"));
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
    fn close_to_new_work_rejects_every_submitted_task_and_nothing_else() {
        let mut r = TaskRegistry::new(4, TaskAcceptance::Queue);
        for id in ["tsk_a", "tsk_b", "tsk_c", "tsk_d"] {
            r.enqueue(id, &format!("ctx_{id}"));
        }
        r.start_task("tsk_a".to_string(), "ctx_tsk_a".to_string(), TaskLane::Bg);
        let (tx, _rx) = oneshot::channel();
        r.set_input_required("tsk_a", "prompt".into(), tx).unwrap();
        assert_eq!(r.request_cancel("tsk_c"), CancelOutcome::Accepted);
        let pending_before = r.pending_count;

        let refused = r.close_to_new_work();

        assert_eq!(
            refused,
            vec![
                ("tsk_b".to_string(), "ctx_tsk_b".to_string()),
                ("tsk_d".to_string(), "ctx_tsk_d".to_string()),
            ]
        );
        for id in ["tsk_b", "tsk_d"] {
            assert_eq!(r.get_task(id).unwrap().status.state, TaskState::Rejected);
        }
        assert_eq!(r.pending_count, pending_before - 2);
        assert_eq!(r.pending_count, 0);
        assert_eq!(
            r.get_task("tsk_a").unwrap().status.state,
            TaskState::InputRequired,
            "a live task that is not submitted is left alone"
        );
        assert_eq!(
            r.get_task("tsk_c").unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(r.request_cancel("tsk_b"), CancelOutcome::AlreadyTerminal);
        assert!(!r.is_canceled("tsk_b"));
    }

    #[test]
    fn a_closed_registry_accepts_nothing_and_closes_once() {
        for (depth, mode) in [(1, TaskAcceptance::Single), (4, TaskAcceptance::Queue)] {
            let mut r = TaskRegistry::new(depth, mode.clone());
            assert!(r.can_accept());
            assert!(!r.is_closed());
            r.enqueue("tsk_a", "ctx_001");
            assert_eq!(r.close_to_new_work().len(), 1);
            assert!(r.is_closed());
            assert!(!r.can_accept(), "{mode:?}: a closed registry refuses work");
            assert!(r.close_to_new_work().is_empty(), "{mode:?}: closes once");
            assert!(!r.can_accept());
        }
    }

    /// `LifecycleConfig::tasks_held_at_once` is what `W-ROS-001` compares a member's callers
    /// against; this registry is what actually rejects a call. Filling a registry the way the
    /// door and the task loop do must accept exactly that many tasks, so a change to either rule
    /// fails here instead of leaving the warning wrong.
    #[test]
    fn tasks_held_at_once_matches_what_the_registry_accepts() {
        const SAFETY_CAP: usize = 64;
        for (mode, depth, expected) in [
            (TaskAcceptance::None, 1, 0),
            (TaskAcceptance::Single, 1, 1),
            (TaskAcceptance::Queue, 0, 0),
            (TaskAcceptance::Queue, 1, 2),
            (TaskAcceptance::Queue, 3, 4),
        ] {
            let mut r = TaskRegistry::new(depth, mode.clone());
            let mut accepted = 0;
            while r.can_accept() && accepted < SAFETY_CAP {
                let task_id = format!("tsk_{accepted}");
                r.enqueue(&task_id, "ctx_001");
                if accepted == 0 {
                    r.start_task(task_id, "ctx_001".to_string(), TaskLane::Peer);
                }
                accepted += 1;
            }
            let lifecycle = murmur_artifact::LifecycleConfig {
                task_acceptance: mode.clone(),
                queue_depth: depth,
                ..Default::default()
            };
            assert_eq!(accepted, expected, "{mode:?} at depth {depth}");
            assert_eq!(
                accepted,
                lifecycle.tasks_held_at_once(),
                "{mode:?} at depth {depth}: tasks_held_at_once drifted from the registry"
            );
        }
    }

    #[test]
    fn as_str_is_the_serialized_spelling() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Rejected,
            TaskState::Canceled,
        ] {
            assert_eq!(
                serde_json::to_value(&state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
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
