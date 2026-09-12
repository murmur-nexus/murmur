//! Ending one running capsule, and reporting what it left behind.

use std::collections::HashSet;
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
        wait_for_cancel_records(&record, result);
    }
    let signal = end_the_process(&record, Duration::from_secs(timeout_secs))?;

    // Only now: the process is gone, and nothing else will remove the record for it. `mur run`
    // dies of `SIGTERM` under the default disposition, so its Rust destructors — the guard that
    // wrote this record included — never run.
    running::prune(&record);

    println!("stopped: {}", record.session_id);
    println!(
        "capsule: {}@{}",
        record.capsule_name, record.capsule_version
    );
    println!("signal:  {signal}");
    match stopped {
        Ok(result) => {
            let canceled = result.get("canceled").and_then(Value::as_array);
            for task_id in canceled.into_iter().flatten().filter_map(Value::as_str) {
                println!("canceled: {task_id}");
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

/// Wait for the capsule to have recorded what the cancellations did, before the signal lands.
///
/// The door records a cancellation and answers immediately; the agent loop is a different task,
/// and the `task_canceled` and `task_end` records it writes when it notices are the only durable
/// account of where the cancel landed. `SIGTERM` follows within microseconds and `mur run` dies of
/// it under the default disposition — no destructor, no flush — so without this wait the account
/// is lost in exactly the case it was asked for, and the trace ends with the task still working.
///
/// Bounded and best-effort. A capsule that cannot finish writing inside
/// [`CANCEL_SETTLE_DEADLINE`] is the one that most needs ending, so the stop proceeds either way.
fn wait_for_cancel_records(record: &RunningRecord, result: &Value) {
    let expected: Vec<&str> = result
        .get("canceled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
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
        let noted = cancels_noted(&trace_path);
        if expected.iter().all(|task_id| noted.contains(*task_id)) {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Every task id the session's trace already carries a `task_canceled` record for.
fn cancels_noted(trace_path: &std::path::Path) -> HashSet<String> {
    let Ok(trace) = std::fs::read_to_string(trace_path) else {
        return HashSet::new();
    };
    trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("event_type").and_then(Value::as_str) == Some("task_canceled"))
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
