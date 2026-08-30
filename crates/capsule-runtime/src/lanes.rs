//! Which queued task runs next.
//!
//! A capsule runs one task at a time. Tasks that arrive while one is running wait, and the
//! order they wait in is decided here: by lane first, by arrival second. A lane is read off the
//! task's own [`TaskOrigin`], so nothing declares a lane and nothing can be filed in one its
//! origin does not name.
//!
//! For a capsule with a single source of tasks this changes nothing — every task lands in the
//! same lane and comes back out in arrival order. It decides something only where two sources
//! are in flight at once.

use std::collections::VecDeque;

use crate::{a2a::IncomingTask, origin::TaskOrigin};

/// Ordering classes the queue chooses between. Declared lowest to highest, so `Ord` is precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskLane {
    /// Work with nobody waiting on it: a timer, a webhook, a finished sub-capsule, the runtime's
    /// own housekeeping.
    Bg,
    /// A message from another capsule, which has a task of its own blocked on the answer.
    Peer,
    /// A person is waiting.
    User,
}

impl TaskLane {
    /// The lane an origin names.
    ///
    /// Exhaustive by construction: a seventh [`TaskOrigin`] fails to compile here rather than
    /// falling into a lane nobody chose for it.
    pub fn for_origin(origin: TaskOrigin) -> Self {
        match origin {
            TaskOrigin::User => Self::User,
            TaskOrigin::Peer => Self::Peer,
            // Nothing is blocked on any of these. `System` is the runtime enqueuing work for
            // itself — a retry, a sweep — which is background work by the same reading.
            TaskOrigin::Schedule | TaskOrigin::Event | TaskOrigin::Completion => Self::Bg,
            TaskOrigin::System => Self::Bg,
        }
    }

    /// The lowercase spelling, as it appears on a `task_start` trace record.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Peer => "peer",
            Self::Bg => "bg",
        }
    }
}

/// The tasks a capsule has accepted but not yet started, one queue per lane.
///
/// Admission is not this type's job: a task reaches [`Self::push`] only after the registry has
/// counted it against `queue_depth`, and it stays counted until `start_task` runs. Filing a task
/// in a lane neither accepts nor refuses anything.
pub(crate) struct LaneQueue {
    user: VecDeque<IncomingTask>,
    peer: VecDeque<IncomingTask>,
    bg: VecDeque<IncomingTask>,
}

impl LaneQueue {
    pub(crate) fn new() -> Self {
        Self {
            user: VecDeque::new(),
            peer: VecDeque::new(),
            bg: VecDeque::new(),
        }
    }

    /// File a task in the lane its own origin names.
    ///
    /// Takes no lane argument, so no caller can misfile a task.
    pub(crate) fn push(&mut self, task: IncomingTask) {
        let lane = TaskLane::for_origin(task.provenance.origin());
        self.lane_mut(lane).push_back(task);
    }

    /// Take the front of the highest non-empty lane, with the lane it came from.
    ///
    /// `active` is the lane of the task the capsule is running, if any. `None` is returned
    /// whenever `active` is `Some` — before any lane is looked at, whatever lane is active and
    /// whatever lanes hold tasks — and whenever every lane is empty. A refusal moves nothing: the
    /// task that would have been chosen is still at the front of its lane afterwards.
    ///
    /// The single call site today asks only between tasks, so the refusal never fires there. It
    /// is here so that a later call site which asks mid-task cannot introduce preemption by
    /// accident.
    pub(crate) fn next(&mut self, active: Option<TaskLane>) -> Option<(TaskLane, IncomingTask)> {
        if active.is_some() {
            return None;
        }
        for lane in [TaskLane::User, TaskLane::Peer, TaskLane::Bg] {
            if let Some(task) = self.lane_mut(lane).pop_front() {
                return Some((lane, task));
            }
        }
        None
    }

    fn lane_mut(&mut self, lane: TaskLane) -> &mut VecDeque<IncomingTask> {
        match lane {
            TaskLane::User => &mut self.user,
            TaskLane::Peer => &mut self.peer,
            TaskLane::Bg => &mut self.bg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::TaskProvenance;

    const ALL_LANES: [TaskLane; 3] = [TaskLane::User, TaskLane::Peer, TaskLane::Bg];

    fn task(id: &str, origin: TaskOrigin) -> IncomingTask {
        IncomingTask {
            task_id: id.to_string(),
            context_id: "ctx_001".to_string(),
            message_id: format!("msg_{id}"),
            message_text: id.to_string(),
            traceparent: None,
            provenance: TaskProvenance::derive(origin, None),
        }
    }

    /// An origin that puts a task in `lane`, for a test that cares about the lane and not about
    /// which of the origins mapping to it was used.
    fn origin_for(lane: TaskLane) -> TaskOrigin {
        match lane {
            TaskLane::User => TaskOrigin::User,
            TaskLane::Peer => TaskOrigin::Peer,
            TaskLane::Bg => TaskOrigin::Event,
        }
    }

    fn drain(queue: &mut LaneQueue) -> Vec<(TaskLane, String)> {
        let mut drained = Vec::new();
        while let Some((lane, task)) = queue.next(None) {
            drained.push((lane, task.task_id));
        }
        drained
    }

    #[test]
    fn every_origin_names_a_lane() {
        for (origin, expected) in [
            (TaskOrigin::User, TaskLane::User),
            (TaskOrigin::Peer, TaskLane::Peer),
            (TaskOrigin::Schedule, TaskLane::Bg),
            (TaskOrigin::Event, TaskLane::Bg),
            (TaskOrigin::Completion, TaskLane::Bg),
            (TaskOrigin::System, TaskLane::Bg),
        ] {
            assert_eq!(
                TaskLane::for_origin(origin),
                expected,
                "{origin:?} belongs in {expected:?}"
            );
        }
    }

    #[test]
    fn lane_order_is_precedence() {
        assert!(TaskLane::User > TaskLane::Peer);
        assert!(TaskLane::Peer > TaskLane::Bg);
        for (lane, spelling) in [
            (TaskLane::User, "user"),
            (TaskLane::Peer, "peer"),
            (TaskLane::Bg, "bg"),
        ] {
            assert_eq!(lane.as_str(), spelling);
        }
    }

    /// Arrival order and lane order disagree on every adjacent pair, so nothing here comes out
    /// right by accident of insertion.
    #[test]
    fn every_lane_drains_before_the_one_below_it() {
        let mut queue = LaneQueue::new();
        for (id, origin) in [
            ("bg-1", TaskOrigin::Schedule),
            ("user-1", TaskOrigin::User),
            ("bg-2", TaskOrigin::Event),
            ("peer-1", TaskOrigin::Peer),
            ("user-2", TaskOrigin::User),
            ("bg-3", TaskOrigin::Completion),
            ("peer-2", TaskOrigin::Peer),
        ] {
            queue.push(task(id, origin));
        }

        assert_eq!(
            drain(&mut queue),
            vec![
                (TaskLane::User, "user-1".to_string()),
                (TaskLane::User, "user-2".to_string()),
                (TaskLane::Peer, "peer-1".to_string()),
                (TaskLane::Peer, "peer-2".to_string()),
                (TaskLane::Bg, "bg-1".to_string()),
                (TaskLane::Bg, "bg-2".to_string()),
                (TaskLane::Bg, "bg-3".to_string()),
            ]
        );
        assert!(
            queue.next(None).is_none(),
            "the queue is empty once drained"
        );
    }

    /// The case the lanes exist for: a person's task arriving last still runs next.
    #[test]
    fn a_user_task_behind_six_completions_runs_first() {
        let mut queue = LaneQueue::new();
        for i in 1..=6 {
            queue.push(task(&format!("done-{i}"), TaskOrigin::Completion));
        }
        queue.push(task("asked", TaskOrigin::User));

        let drained = drain(&mut queue);
        assert_eq!(drained[0], (TaskLane::User, "asked".to_string()));
        assert_eq!(
            drained[1..],
            (1..=6)
                .map(|i| (TaskLane::Bg, format!("done-{i}")))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tasks_of_one_origin_drain_in_push_order() {
        let mut queue = LaneQueue::new();
        for id in ["first", "second", "third"] {
            queue.push(task(id, TaskOrigin::Peer));
        }
        assert_eq!(
            drain(&mut queue),
            vec![
                (TaskLane::Peer, "first".to_string()),
                (TaskLane::Peer, "second".to_string()),
                (TaskLane::Peer, "third".to_string()),
            ]
        );
    }

    /// No lane preempts any other, including its own, and a refusal costs the queued task
    /// nothing.
    #[test]
    fn nothing_is_yielded_while_a_task_is_active() {
        for active in ALL_LANES {
            for queued in ALL_LANES {
                let mut queue = LaneQueue::new();
                queue.push(task("waiting", origin_for(queued)));
                assert!(
                    queue.next(Some(active)).is_none(),
                    "a {queued:?} task must not preempt an active {active:?} task"
                );
                assert_eq!(
                    queue.next(None).map(|(lane, task)| (lane, task.task_id)),
                    Some((queued, "waiting".to_string())),
                    "the refused {queued:?} task must still be queued afterwards"
                );
            }
        }
    }
}
