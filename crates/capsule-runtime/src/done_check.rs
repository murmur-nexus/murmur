//! `lifecycle.done_check`: what one run of the operator's command reports, whether it reopens the
//! task, and the feedback a reopened attempt is sent.
//!
//! The runtime never judges the work itself. A run's exit status is the whole verdict, and the
//! feedback is the run's facts — the command, its exit code and its capped output — with no
//! wording about what to do next.

use crate::agent::AgentLoopExit;
use crate::errors::RuntimeError;
use crate::shell::{ForegroundRun, ForegroundStopReason, ShellExecError};

/// How one run of the command went, recorded as `done_check.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoneCheckOutcome {
    /// Exit 0.
    Passed,
    /// Any other exit, a resource-limit kill included.
    Failed,
    /// Killed by the runtime at `lifecycle.done_check.timeout_secs`.
    TimedOut,
    /// Killed because the task's cancel was raised: a person's cancel or the session deadline.
    Stopped,
    /// The command could not be started.
    NotStarted,
}

impl DoneCheckOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Stopped => "stopped",
            Self::NotStarted => "not_started",
        }
    }
}

/// What ended a task with its check unmet, recorded as `task_end.done_check.stopped_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoneCheckStop {
    /// `lifecycle.done_check.limit` reopens were spent and the next run failed too.
    Limit,
    /// `inference.max_turns` was spent, at the run or inside a reopened attempt.
    MaxTurns,
    /// A spend ceiling refused the next driver call of a reopened attempt.
    SpendCeiling,
    /// The session deadline fell while the check ran or a reopened attempt worked.
    Deadline,
}

impl DoneCheckStop {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::MaxTurns => "max_turns",
            Self::SpendCeiling => "spend_ceiling",
            Self::Deadline => "deadline",
        }
    }
}

/// What a run that neither a cancel nor the deadline stopped means for the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoneCheckVerdict {
    /// The check passed: the task ends.
    Met,
    /// The check failed and a reopen remains: the task is reopened with the run's feedback.
    Reopen,
    /// The check failed and the task ends anyway, for this reason.
    Unmet(DoneCheckStop),
}

/// The verdict on a run: `reopens_used` is the reopens the check has caused in this task so far,
/// `limit` is `lifecycle.done_check.limit`, and `turns_left` is what `inference.max_turns` leaves
/// (`None` with no cap). The limit is checked before the turns.
pub(crate) fn decide(
    outcome: DoneCheckOutcome,
    reopens_used: u32,
    limit: u32,
    turns_left: Option<u32>,
) -> DoneCheckVerdict {
    if outcome == DoneCheckOutcome::Passed {
        DoneCheckVerdict::Met
    } else if reopens_used >= limit {
        DoneCheckVerdict::Unmet(DoneCheckStop::Limit)
    } else if turns_left == Some(0) {
        DoneCheckVerdict::Unmet(DoneCheckStop::MaxTurns)
    } else {
        DoneCheckVerdict::Reopen
    }
}

/// How the check has gone across a task's attempts, for `task_end.done_check`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DoneCheckTally {
    /// Runs, across reopens of either kind.
    pub(crate) runs: u32,
    /// Reopens the check caused.
    pub(crate) reopens: u32,
    /// Whether the last run passed; `None` before the first run.
    pub(crate) met: Option<bool>,
    /// What ended the task with the check unmet.
    pub(crate) stopped_by: Option<DoneCheckStop>,
}

impl DoneCheckTally {
    pub(crate) fn summary(&self, limit: u32) -> crate::trace::DoneCheckSummary {
        crate::trace::DoneCheckSummary {
            runs: self.runs,
            reopens: self.reopens,
            limit,
            met: self.met,
            stopped_by: self.stopped_by.map(DoneCheckStop::as_str),
        }
    }
}

/// Whether an attempt reopened by the check ended on a budget, which ends the task with the
/// answer that preceded the reopen. `stopped_at_deadline` is whether the task registry recorded
/// the task as stopped by the session deadline.
pub(crate) fn budget_stop(
    result: &Result<AgentLoopExit, RuntimeError>,
    stopped_at_deadline: bool,
) -> Option<DoneCheckStop> {
    match result {
        Ok(AgentLoopExit::MaxTurnsReached) => Some(DoneCheckStop::MaxTurns),
        Ok(AgentLoopExit::SpendCeilingReached) => Some(DoneCheckStop::SpendCeiling),
        Ok(AgentLoopExit::Canceled) if stopped_at_deadline => Some(DoneCheckStop::Deadline),
        _ => None,
    }
}

/// One run of the command, as the record and the feedback describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoneCheckReport {
    pub(crate) outcome: DoneCheckOutcome,
    /// `None` for `timed_out`, `stopped` and `not_started`.
    pub(crate) exit_code: Option<i32>,
    /// Capped at 16 KiB, as the shell tool caps it.
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) duration_ms: u64,
    pub(crate) truncated: bool,
    /// Where the full output went when it was truncated.
    pub(crate) full_output_path: Option<String>,
    /// The `capabilities.resources` field whose limit killed the command.
    pub(crate) resource_limit: Option<String>,
    /// Why the command did not start, for `not_started`.
    pub(crate) not_started: Option<String>,
}

impl DoneCheckReport {
    /// The report of a run [`crate::shell::run_shell_until_stopped`] returned. A failure that ends
    /// the session (the sealed root could not be built) is the `Err`, as it is for the shell tool.
    pub(crate) fn from_run(
        run: Result<ForegroundRun, ShellExecError>,
        duration_ms: u64,
    ) -> Result<Self, RuntimeError> {
        let run = match run {
            Ok(run) => run,
            Err(error) => {
                if let Some(fatal) = error.session_fatal() {
                    return Err(fatal);
                }
                return Ok(Self {
                    outcome: DoneCheckOutcome::NotStarted,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    duration_ms,
                    truncated: false,
                    full_output_path: None,
                    resource_limit: None,
                    not_started: Some(error.to_string()),
                });
            }
        };
        let result = run.result;
        let (outcome, exit_code) = match run.stopped {
            Some(ForegroundStopReason::Canceled) => (DoneCheckOutcome::Stopped, None),
            Some(ForegroundStopReason::TimedOut) => (DoneCheckOutcome::TimedOut, None),
            None if result.exit_code == 0 => (DoneCheckOutcome::Passed, Some(0)),
            None => (DoneCheckOutcome::Failed, Some(result.exit_code)),
        };
        Ok(Self {
            outcome,
            exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
            duration_ms: result.duration_ms,
            truncated: result.truncated,
            full_output_path: result.full_output_path,
            resource_limit: result.resource_limit_hit,
            not_started: None,
        })
    }

    /// The part of the feedback that is fenced: the command as declared, then the exit code and
    /// both streams the way the shell tool reports a command, or why it did not start.
    pub(crate) fn fenced_body(&self, command: &str) -> String {
        if let Some(error) = &self.not_started {
            return format!("$ {command}\nDid not start: {error}");
        }
        let exit_code = self
            .exit_code
            .map_or_else(|| "none".to_string(), |code| code.to_string());
        let mut body = format!(
            "$ {command}\nExit code: {exit_code}\nStdout:\n{}\nStderr:\n{}",
            self.stdout, self.stderr
        );
        if self.truncated {
            match &self.full_output_path {
                Some(path) => body.push_str(&format!(
                    "\n\nOutput truncated. Full output written to {path}"
                )),
                None => body.push_str("\n\nOutput truncated."),
            }
        }
        body
    }
}

/// The user message a reopen by the check sends: a `Done check` line, `fenced` (the
/// [`DoneCheckReport::fenced_body`] already wrapped under `done-check`), then one line each for a
/// resource limit and the timeout, when they apply. Nothing else.
pub(crate) fn feedback(fenced: &str, report: &DoneCheckReport, timeout_secs: u64) -> String {
    let mut message = format!("Done check\n{fenced}");
    if let Some(limit) = &report.resource_limit {
        message.push_str(&format!("\nResource limit: capabilities.resources.{limit}"));
    }
    if report.outcome == DoneCheckOutcome::TimedOut {
        message.push_str(&format!(
            "\nStopped after {timeout_secs} s by lifecycle.done_check.timeout_secs."
        ));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fence::{done_check_source, wrap_untrusted};

    #[test]
    fn outcomes_and_stops_read_as_their_trace_names() {
        assert_eq!(DoneCheckOutcome::Passed.as_str(), "passed");
        assert_eq!(DoneCheckOutcome::Failed.as_str(), "failed");
        assert_eq!(DoneCheckOutcome::TimedOut.as_str(), "timed_out");
        assert_eq!(DoneCheckOutcome::Stopped.as_str(), "stopped");
        assert_eq!(DoneCheckOutcome::NotStarted.as_str(), "not_started");
        assert_eq!(DoneCheckStop::Limit.as_str(), "limit");
        assert_eq!(DoneCheckStop::MaxTurns.as_str(), "max_turns");
        assert_eq!(DoneCheckStop::SpendCeiling.as_str(), "spend_ceiling");
        assert_eq!(DoneCheckStop::Deadline.as_str(), "deadline");
    }

    #[test]
    fn a_passing_run_is_met_whatever_the_budgets() {
        assert_eq!(
            decide(DoneCheckOutcome::Passed, 3, 3, Some(0)),
            DoneCheckVerdict::Met
        );
        assert_eq!(
            decide(DoneCheckOutcome::Passed, 0, 0, None),
            DoneCheckVerdict::Met
        );
    }

    #[test]
    fn a_failing_run_under_every_budget_reopens() {
        for outcome in [
            DoneCheckOutcome::Failed,
            DoneCheckOutcome::TimedOut,
            DoneCheckOutcome::NotStarted,
        ] {
            assert_eq!(decide(outcome, 0, 1, None), DoneCheckVerdict::Reopen);
            assert_eq!(decide(outcome, 2, 3, Some(1)), DoneCheckVerdict::Reopen);
        }
    }

    #[test]
    fn a_failing_run_at_the_limit_is_unmet_at_the_limit_before_the_turns() {
        assert_eq!(
            decide(DoneCheckOutcome::Failed, 2, 2, Some(5)),
            DoneCheckVerdict::Unmet(DoneCheckStop::Limit)
        );
        assert_eq!(
            decide(DoneCheckOutcome::Failed, 2, 2, Some(0)),
            DoneCheckVerdict::Unmet(DoneCheckStop::Limit)
        );
        // `limit: 0` observes and never reopens.
        assert_eq!(
            decide(DoneCheckOutcome::Failed, 0, 0, None),
            DoneCheckVerdict::Unmet(DoneCheckStop::Limit)
        );
    }

    #[test]
    fn a_failing_run_with_no_turn_left_is_unmet_at_max_turns() {
        assert_eq!(
            decide(DoneCheckOutcome::TimedOut, 1, 5, Some(0)),
            DoneCheckVerdict::Unmet(DoneCheckStop::MaxTurns)
        );
    }

    #[test]
    fn a_reopened_attempt_that_spent_a_budget_is_a_budget_stop() {
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::MaxTurnsReached), false),
            Some(DoneCheckStop::MaxTurns)
        );
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::SpendCeilingReached), false),
            Some(DoneCheckStop::SpendCeiling)
        );
        assert_eq!(
            budget_stop(&Ok(AgentLoopExit::Canceled), true),
            Some(DoneCheckStop::Deadline)
        );
        assert_eq!(budget_stop(&Ok(AgentLoopExit::Canceled), false), None);
        assert_eq!(budget_stop(&Ok(AgentLoopExit::Ok), true), None);
        assert_eq!(budget_stop(&Ok(AgentLoopExit::Failed), true), None);
    }

    fn report(outcome: DoneCheckOutcome, exit_code: Option<i32>) -> DoneCheckReport {
        DoneCheckReport {
            outcome,
            exit_code,
            stdout: "1 test failed\n".into(),
            stderr: String::new(),
            duration_ms: 40,
            truncated: false,
            full_output_path: None,
            resource_limit: None,
            not_started: None,
        }
    }

    fn message(report: &DoneCheckReport, timeout_secs: u64) -> String {
        let fenced = wrap_untrusted(done_check_source(), &report.fenced_body("cargo test"));
        feedback(&fenced, report, timeout_secs)
    }

    #[test]
    fn the_feedback_is_the_command_its_exit_code_and_its_output_fenced() {
        assert_eq!(
            message(&report(DoneCheckOutcome::Failed, Some(1)), 600),
            "Done check\n<untrusted-content source=done-check>\n$ cargo test\nExit code: 1\n\
             Stdout:\n1 test failed\n\nStderr:\n\n</untrusted-content>"
        );
    }

    #[test]
    fn truncated_output_says_so_inside_the_fence() {
        let mut with_path = report(DoneCheckOutcome::Failed, Some(1));
        with_path.truncated = true;
        with_path.full_output_path = Some("/records/shell-1.log".into());
        assert!(message(&with_path, 600).ends_with(
            "Stderr:\n\n\nOutput truncated. Full output written to /records/shell-1.log\n\
             </untrusted-content>"
        ));
        let mut without_path = with_path.clone();
        without_path.full_output_path = None;
        assert!(message(&without_path, 600)
            .ends_with("Stderr:\n\n\nOutput truncated.\n</untrusted-content>"));
    }

    #[test]
    fn a_timeout_reads_exit_code_none_and_a_stop_line_after_the_fence() {
        let timed_out = report(DoneCheckOutcome::TimedOut, None);
        let text = message(&timed_out, 2);
        assert!(text.contains("\nExit code: none\n"), "{text}");
        assert!(
            text.ends_with(
                "</untrusted-content>\nStopped after 2 s by lifecycle.done_check.timeout_secs."
            ),
            "{text}"
        );
    }

    #[test]
    fn a_resource_limit_is_named_after_the_fence_without_advice() {
        let mut killed = report(DoneCheckOutcome::Failed, Some(152));
        killed.resource_limit = Some("cpu_seconds".into());
        let text = message(&killed, 600);
        assert!(
            text.ends_with(
                "</untrusted-content>\nResource limit: capabilities.resources.cpu_seconds"
            ),
            "{text}"
        );
        assert!(!text.to_lowercase().contains("retry"), "{text}");
    }

    #[test]
    fn a_command_that_did_not_start_says_why() {
        let mut not_started = report(DoneCheckOutcome::NotStarted, None);
        not_started.not_started = Some("binary 'cargo' is not in capabilities.shell.allow".into());
        assert_eq!(
            message(&not_started, 600),
            "Done check\n<untrusted-content source=done-check>\n$ cargo test\nDid not start: \
             binary 'cargo' is not in capabilities.shell.allow\n</untrusted-content>"
        );
    }

    #[test]
    fn a_sealed_root_failure_ends_the_session_and_any_other_error_is_not_started() {
        let fatal = DoneCheckReport::from_run(
            Err(ShellExecError::SealedRootConstructionFailed {
                detail: "mount".into(),
            }),
            0,
        );
        assert!(matches!(
            fatal,
            Err(RuntimeError::SealedRootConstructionFailed { .. })
        ));
        let not_started =
            DoneCheckReport::from_run(Err(ShellExecError::Failed("no such file".into())), 3)
                .unwrap();
        assert_eq!(not_started.outcome, DoneCheckOutcome::NotStarted);
        assert_eq!(not_started.exit_code, None);
        assert_eq!(not_started.not_started.as_deref(), Some("no such file"));
    }
}
