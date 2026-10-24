//! `lifecycle.keep_working`: when a task kept working past a turn end stops being sent on.
//!
//! The runtime never judges whether the work is done. These functions only read budgets: how
//! many continuations the task has had, the turns left under `inference.max_turns`, whether the
//! conversation can be resumed, and how a continued attempt ended.

use crate::agent::AgentLoopExit;
use crate::errors::RuntimeError;

/// What stopped a task from being kept working, recorded as `task_end.keep_working.stopped_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepWorkingStop {
    /// `lifecycle.keep_working.limit` continuations were sent and the next turn ended.
    Limit,
    /// `inference.max_turns` was spent, at a turn end or inside a continued attempt.
    MaxTurns,
    /// A spend ceiling refused the next driver call of a continued attempt.
    SpendCeiling,
    /// The session deadline fell while the task was being kept working.
    Deadline,
    /// `transport: process` left no harness session to resume.
    NoSession,
}

impl KeepWorkingStop {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::MaxTurns => "max_turns",
            Self::SpendCeiling => "spend_ceiling",
            Self::Deadline => "deadline",
            Self::NoSession => "no_session",
        }
    }
}

/// At a turn end, whether to send another continuation: `None` sends one, `Some` names what
/// stops the task. Checked in order: `used` has reached `limit`, no turn is left under
/// `inference.max_turns` (`turns_left` is `None` with no cap), the conversation cannot continue.
pub(crate) fn stop_at_turn_end(
    used: u32,
    limit: u32,
    turns_left: Option<u32>,
    can_continue: bool,
) -> Option<KeepWorkingStop> {
    if used >= limit {
        Some(KeepWorkingStop::Limit)
    } else if turns_left == Some(0) {
        Some(KeepWorkingStop::MaxTurns)
    } else if !can_continue {
        Some(KeepWorkingStop::NoSession)
    } else {
        None
    }
}

/// Whether a continued attempt ended on a budget, which ends the task with the answer it already
/// holds. `stopped_at_deadline` is whether the task registry recorded the task as stopped by the
/// session deadline; a `Canceled` attempt it did not stop (a person's cancel, `SIGTERM`, the
/// formation's end) is `None`, as is every other ending.
pub(crate) fn budget_stop(
    result: &Result<AgentLoopExit, RuntimeError>,
    stopped_at_deadline: bool,
) -> Option<KeepWorkingStop> {
    match result {
        Ok(AgentLoopExit::MaxTurnsReached) => Some(KeepWorkingStop::MaxTurns),
        Ok(AgentLoopExit::SpendCeilingReached) => Some(KeepWorkingStop::SpendCeiling),
        Ok(AgentLoopExit::Canceled) if stopped_at_deadline => Some(KeepWorkingStop::Deadline),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_read_as_their_trace_names() {
        assert_eq!(KeepWorkingStop::Limit.as_str(), "limit");
        assert_eq!(KeepWorkingStop::MaxTurns.as_str(), "max_turns");
        assert_eq!(KeepWorkingStop::SpendCeiling.as_str(), "spend_ceiling");
        assert_eq!(KeepWorkingStop::Deadline.as_str(), "deadline");
        assert_eq!(KeepWorkingStop::NoSession.as_str(), "no_session");
    }

    #[test]
    fn a_turn_end_under_every_budget_is_sent_on() {
        assert_eq!(stop_at_turn_end(0, 1, None, true), None);
        assert_eq!(stop_at_turn_end(1, 3, Some(1), true), None);
    }

    #[test]
    fn a_turn_end_after_limit_continuations_stops_at_the_limit() {
        assert_eq!(
            stop_at_turn_end(2, 2, Some(5), true),
            Some(KeepWorkingStop::Limit)
        );
        // The limit is checked first, before the turn budget and the session.
        assert_eq!(
            stop_at_turn_end(2, 2, Some(0), false),
            Some(KeepWorkingStop::Limit)
        );
    }

    #[test]
    fn a_turn_end_with_no_turn_left_stops_at_max_turns() {
        assert_eq!(
            stop_at_turn_end(0, 5, Some(0), true),
            Some(KeepWorkingStop::MaxTurns)
        );
        assert_eq!(
            stop_at_turn_end(0, 5, Some(0), false),
            Some(KeepWorkingStop::MaxTurns)
        );
    }

    #[test]
    fn a_turn_end_that_cannot_continue_stops_with_no_session() {
        assert_eq!(
            stop_at_turn_end(0, 5, None, false),
            Some(KeepWorkingStop::NoSession)
        );
    }

    #[test]
    fn a_continued_attempt_that_spent_a_budget_is_a_budget_stop() {
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::MaxTurnsReached), false),
            Some(KeepWorkingStop::MaxTurns)
        );
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::SpendCeilingReached), false),
            Some(KeepWorkingStop::SpendCeiling)
        );
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::Canceled), true),
            Some(KeepWorkingStop::Deadline)
        );
    }

    #[test]
    fn every_other_ending_of_a_continued_attempt_is_reported_as_it_is() {
        assert_eq!(budget_stop(&Ok(AgentLoopExit::Canceled), false), None);
        for exit in [
            AgentLoopExit::Ok,
            AgentLoopExit::Failed,
            AgentLoopExit::NoAnswer,
        ] {
            assert_eq!(budget_stop(&Ok(exit), true), None);
        }
        assert_eq!(
            budget_stop(&Err(RuntimeError::AgentLoopFailed("boom".into())), true),
            None
        );
    }
}
