//! Ending one running capsule, and reporting what it left behind.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use capsule_runtime::{running, ProcessState, RunningRecord, SignalOutcome};
use serde_json::Value;

use crate::error::{CliError, E_RUN_024};
use crate::live_address::resolve_live_process;
use crate::residue::print_residue;

/// How often the grace period is checked for the process having exited.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long the capsule is given to record what the cancel did before the signal lands. Reached
/// only when the capsule is wedged; the usual wait is one poll.
const CANCEL_SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// The `phase` a `task_canceled` record carries when its task was cancelled before it started —
/// the runtime's `cancel::PHASE_QUEUED`, which is not exported.
const PHASE_QUEUED: &str = "queued";

/// How long a `SIGKILL` is given to take effect before the session is reported as unendable.
/// `SIGKILL` is not deliverable to a process this user may not signal and not catchable by one it
/// may, so this is a bound on the kernel reaping the process, not on the process cooperating.
const KILL_DEADLINE: Duration = Duration::from_secs(5);

/// The three residue outcomes, as three lines that cannot be mistaken for one another.
///
/// "it left nothing behind" and "it could not be asked what it left behind" are different facts
/// about the machine, and an empty list renders both. Only the first is an answer.
const RESIDUE_NONE: &str = "residue: nothing else was left running";
const RESIDUE_UNKNOWN_PREFIX: &str = "residue: unknown — the capsule could not be asked:";

/// Printed after `unended: <task_id>` for a cancelled task whose ending the trace still lacks once
/// the process is gone. A capsule that recorded every ending before it exited prints no such line.
const UNENDED_SENTENCE: &str = "the trace does not record how this task ended";

/// End one running capsule: ask its door, then signal, then escalate.
///
/// The order is the point. `session/stop` goes first because it is the only moment anything can
/// ask the capsule what it leaves running — a detached shell command keeps its own lifecycle and
/// a delegated child is still going, and the registries that know about either die with the
/// process. Reaching for `kill` first would throw that account away.
///
/// A door that does not answer stops nothing: the two signalling steps still run, and the missing
/// account is reported as missing. Exit 0 either way — the session was ended; only the accounting
/// is incomplete.
///
/// `timeout_secs` is the grace period between `SIGTERM` and `SIGKILL`. `0` escalates immediately,
/// with no grace at all.
pub(crate) fn run_stop(address: &str, timeout_secs: u64) -> Result<(), CliError> {
    // Layer 2 only. A quiet door is not a reason to refuse a stop, so the verification that
    // decides whether this address names something stoppable stops short of asking it.
    let record = resolve_live_process(address)?;

    let stopped = ask_the_door(&record);
    if let Ok(result) = &stopped {
        wait_for_endings(&record, result);
    }
    let signal = end_the_process(&record, Duration::from_secs(timeout_secs))?;

    // The process is gone, so the trace is final: whatever ending is missing now stays missing.
    let unended: Vec<String> = match &stopped {
        Ok(result) => {
            let trace_path = record.workdir.join("trace.jsonl");
            if trace_path.exists() {
                let settled = endings_noted(&trace_path);
                canceled_ids(result)
                    .into_iter()
                    .filter(|task_id| !settled.contains(*task_id))
                    .map(str::to_string)
                    .collect()
            } else {
                Vec::new()
            }
        }
        Err(_) => Vec::new(),
    };

    // A capsule that ran its `SIGTERM` teardown removed its own record, and pruning an absent
    // record does nothing. One ended by `SIGKILL`, or by its own teardown deadline, never removes
    // it, and nothing but this will.
    running::prune(&record);

    println!("stopped: {}", record.session_id);
    println!(
        "capsule: {}@{}",
        record.capsule_name, record.capsule_version
    );
    println!("signal:  {signal}");
    match stopped {
        Ok(result) => {
            for task_id in canceled_ids(&result) {
                println!("canceled: {task_id}");
            }
            for task_id in &unended {
                println!("unended: {task_id}  {UNENDED_SENTENCE}");
            }
            let residue = result
                .get("residue")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if residue.is_empty() {
                println!("{RESIDUE_NONE}");
            } else {
                print_residue(&residue);
            }
        }
        Err(reason) => println!("{RESIDUE_UNKNOWN_PREFIX} {reason}"),
    }

    Ok(())
}

/// `session/stop` through the door: cancel what the session still holds, and read the residue.
///
/// The `Err` is why the door could not answer, which is printed rather than returned — a capsule
/// that has stopped listening still has a process to end.
fn ask_the_door(record: &RunningRecord) -> Result<Value, String> {
    let addr = record
        .url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/stop",
        "params": {}
    })
    .to_string();

    let response = super::cancel::post_json(addr, &body).map_err(|e| e.message)?;
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the capsule refused session/stop");
        return Err(format!("the capsule at {addr} refused the stop: {message}"));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("the capsule at {addr} answered with neither a result nor an error"))
}

/// The task ids the door's `session/stop` answer says it cancelled, in the order it gave them.
fn canceled_ids(result: &Value) -> Vec<&str> {
    result
        .get("canceled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Wait until every cancelled task's ending is in the trace, before the signal lands.
///
/// The door records a cancellation and answers immediately; the agent loop is a different task,
/// and the records it writes when it notices are the only durable account of where the cancel
/// landed. A task that was running writes `task_canceled` first and `task_end` after it, with the
/// `on-task-end` hooks in between, so `task_canceled` alone is one record too few: a signal that
/// lands after it and before `task_end` leaves a trace that never says how the task ended. A task
/// cancelled before it started writes only `task_canceled` with phase `queued`, which is its
/// ending — see [`endings_noted`].
///
/// Bounded and best-effort. A capsule that cannot finish writing inside
/// [`CANCEL_SETTLE_DEADLINE`] is the one that most needs ending, so the stop proceeds either way,
/// and the ending that never arrived is reported as `unended:`.
fn wait_for_endings(record: &RunningRecord, result: &Value) {
    let expected = canceled_ids(result);
    if expected.is_empty() {
        return;
    }

    let trace_path = record.workdir.join("trace.jsonl");
    // A session tracing somewhere else, or not at all, has no record here to wait for and never
    // will. Waiting out the deadline on every such stop would be a delay that buys nothing.
    if !trace_path.exists() {
        return;
    }

    let deadline = Instant::now() + CANCEL_SETTLE_DEADLINE;
    loop {
        let settled = endings_noted(&trace_path);
        if expected.iter().all(|task_id| settled.contains(*task_id)) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Every task id whose ending the session's trace already carries.
///
/// A task is settled by a `task_end` with any `exit_status`, or by a `task_canceled` whose `phase`
/// is `queued`: a task cancelled before it started never runs, so it never writes `task_end`. A
/// `task_canceled` in any other phase settles nothing, because its `task_end` is still to come.
/// A missing file settles nothing, and a line that does not parse — the one being written — is
/// skipped.
fn endings_noted(trace_path: &Path) -> HashSet<String> {
    let Ok(trace) = std::fs::read_to_string(trace_path) else {
        return HashSet::new();
    };
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(
            |event| match event.get("event_type").and_then(Value::as_str) {
                Some("task_end") => true,
                Some("task_canceled") => {
                    event.get("phase").and_then(Value::as_str) == Some(PHASE_QUEUED)
                }
                _ => false,
            },
        )
        .filter_map(|event| {
            event
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// `SIGTERM`, then `SIGKILL` once the grace period is out. Returns what actually ended it.
///
/// Every `kill(2)` re-runs layers 1 and 2 inside [`running::signal_term`] and
/// [`running::signal_kill`], so a pid that has been inherited by an unrelated process since this
/// command started is not signalled.
fn end_the_process(record: &RunningRecord, grace: Duration) -> Result<&'static str, CliError> {
    match running::signal_term(record) {
        SignalOutcome::AlreadyGone => return Ok("none — the process had already exited"),
        SignalOutcome::Refused(reason) => {
            return Err(unendable(
                record,
                &format!("the kernel refused SIGTERM to pid {}: {reason}", record.pid),
            ))
        }
        SignalOutcome::Sent => {}
    }
    if wait_for_exit(record, grace) {
        return Ok("SIGTERM");
    }

    match running::signal_kill(record) {
        // It exited between the grace period expiring and the signal — `SIGTERM` did the work.
        SignalOutcome::AlreadyGone => return Ok("SIGTERM"),
        SignalOutcome::Refused(reason) => {
            return Err(unendable(
                record,
                &format!("the kernel refused SIGKILL to pid {}: {reason}", record.pid),
            ))
        }
        SignalOutcome::Sent => {}
    }
    if wait_for_exit(record, KILL_DEADLINE) {
        return Ok("SIGKILL");
    }
    Err(unendable(
        record,
        &format!(
            "pid {} was still running {} seconds after SIGKILL",
            record.pid,
            KILL_DEADLINE.as_secs()
        ),
    ))
}

/// Whether the process is gone before `grace` runs out. A zero grace checks once and returns.
fn wait_for_exit(record: &RunningRecord, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(running::process_state(record), ProcessState::Gone(_)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL.min(grace));
    }
}

/// The session is still running and this command could not end it.
///
/// Its record is deliberately left in place: the capsule it names is still there, and removing
/// the only handle to it would be the opposite of what was asked for.
fn unendable(record: &RunningRecord, reason: &str) -> CliError {
    CliError::with_hint(
        E_RUN_024,
        format!("{} could not be ended: {reason}", record.session_id),
        "the capsule is still running and its record is kept; the process may belong to another \
         user",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_with(lines: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        (dir, path)
    }

    #[test]
    fn a_task_end_settles_its_task() {
        let (_dir, path) = trace_with(&[
            r#"{"event_type":"task_canceled","task_id":"tsk_a","phase":"inference"}"#,
            r#"{"event_type":"task_end","task_id":"tsk_a","exit_status":"canceled"}"#,
        ]);
        assert_eq!(endings_noted(&path), HashSet::from(["tsk_a".to_string()]));
    }

    #[test]
    fn a_queued_cancel_settles_its_task() {
        let (_dir, path) =
            trace_with(&[r#"{"event_type":"task_canceled","task_id":"tsk_q","phase":"queued"}"#]);
        assert_eq!(endings_noted(&path), HashSet::from(["tsk_q".to_string()]));
    }

    #[test]
    fn a_cancel_in_any_other_phase_does_not_settle_its_task() {
        let (_dir, path) = trace_with(&[
            r#"{"event_type":"task_canceled","task_id":"tsk_i","phase":"inference"}"#,
            r#"{"event_type":"task_canceled","task_id":"tsk_t","phase":"turn"}"#,
        ]);
        assert!(endings_noted(&path).is_empty());
    }

    #[test]
    fn a_torn_last_line_and_a_missing_file_are_tolerated() {
        let (dir, path) = trace_with(&[
            r#"{"event_type":"task_end","task_id":"tsk_a","exit_status":"canceled"}"#,
            r#"{"event_type":"task_end","task_id":"tsk_b","exit_st"#,
        ]);
        assert_eq!(endings_noted(&path), HashSet::from(["tsk_a".to_string()]));
        assert!(endings_noted(&dir.path().join("absent.jsonl")).is_empty());
    }

    #[test]
    fn unrelated_events_settle_nothing() {
        let (_dir, path) = trace_with(&[
            r#"{"event_type":"task_start","task_id":"tsk_s"}"#,
            r#"{"event_type":"session_end","exit_status":"ok"}"#,
            r#"{"event_type":"task_end","task_id":"tsk_other","exit_status":"ok"}"#,
        ]);
        let settled = endings_noted(&path);
        assert_eq!(settled, HashSet::from(["tsk_other".to_string()]));
        assert!(!settled.contains("tsk_s"));
    }
}
