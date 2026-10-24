//! `lifecycle.keep_working`: when a task kept working past a turn end stops being sent on.
//!
//! The runtime never judges whether the work is done. These functions only read budgets: how
//! many continuations the task has had, the turns left under `inference.max_turns` and whether the
//! conversation can be resumed. How a continued attempt ended is [`crate::agent::budget_stop`]'s.

use crate::agent::BudgetStop;

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

impl From<BudgetStop> for KeepWorkingStop {
    fn from(stop: BudgetStop) -> Self {
        match stop {
            BudgetStop::MaxTurns => Self::MaxTurns,
            BudgetStop::SpendCeiling => Self::SpendCeiling,
            BudgetStop::Deadline => Self::Deadline,
        }
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
}
