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

use crate::{lanes::TaskLane, origin::TaskProvenance};

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
    /// [`crate::delegation::DELEGATION_ID_HEADER`], or one this runtime's own deadline or
    /// released-child watcher reported. `None` for every other task, including a locally produced
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
