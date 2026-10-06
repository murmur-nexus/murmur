//! Ending one running capsule, or every member of a formation, and reporting what was left behind.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use capsule_runtime::errors::RuntimeError;
use capsule_runtime::formation_launch::MEMBER_STOP_GRACE;
use capsule_runtime::running::ProcessIdentity;
use capsule_runtime::{running, FormationId, ProcessState, RunningRecord, SignalOutcome};
use serde_json::Value;

use crate::error::{CliError, E_RUN_022, E_RUN_024};
use crate::live_address::{records_unreadable, resolve_live_process};
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

/// End what `address` names: one running capsule, or every member of a formation when `address`
/// is a formation id.
///
/// `timeout_secs` is the grace period between `SIGTERM` and `SIGKILL` for each process this
/// command signals itself. `0` escalates immediately, with no grace at all.
pub(crate) fn run_stop(address: &str, timeout_secs: u64) -> Result<(), CliError> {
    let grace = Duration::from_secs(timeout_secs);
    if address.starts_with(FORMATION_ID_PREFIX) {
        return stop_formation(address, grace);
    }
    // Layer 2 only. A quiet door is not a reason to refuse a stop, so the verification that
    // decides whether this address names something stoppable stops short of asking it.
    let record = resolve_live_process(address)?;
    let stopped = stop_session(&record, grace)?;
    print_session_stop(&stopped);
    Ok(())
}

/// What stopping one session did, for [`print_session_stop`].
struct SessionStop {
    session_id: String,
    capsule: String,
    /// What actually ended the process.
    signal: &'static str,
    /// The door's `session/stop` answer, or why it could not be had.
    door: Result<Value, String>,
    /// Cancelled tasks whose ending the final trace does not record.
    unended: Vec<String>,
}

/// End one running capsule: ask its door, then signal, then escalate, then remove its record.
///
/// The order is the point. `session/stop` goes first because it is the only moment anything can
/// ask the capsule what it leaves running — a detached shell command keeps its own lifecycle and
/// a delegated child is still going, and the registries that know about either die with the
/// process. Reaching for `kill` first would throw that account away.
///
/// A door that does not answer stops nothing: the two signalling steps still run, and the missing
/// account is reported as missing. The session was ended either way; only the accounting is
/// incomplete. The `Err` is `E-RUN-024`, the process still running, with its record kept.
fn stop_session(record: &RunningRecord, grace: Duration) -> Result<SessionStop, CliError> {
    let door = ask_the_door(record);
    if let Ok(result) = &door {
        wait_for_endings(record, result);
    }
    let signal = end_the_process(record, grace)?;

    // The process is gone, so the trace is final: whatever ending is missing now stays missing.
    let unended: Vec<String> = match &door {
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
    running::prune(record);

    Ok(SessionStop {
        session_id: record.session_id.clone(),
        capsule: format!("{}@{}", record.capsule_name, record.capsule_version),
        signal,
        door,
        unended,
    })
}

/// The block `mur stop <session>` prints, and `mur stop <formation id>` prints once per member
/// started by hand.
fn print_session_stop(stopped: &SessionStop) {
    println!("stopped: {}", stopped.session_id);
    println!("capsule: {}", stopped.capsule);
    println!("signal:  {}", stopped.signal);
    match &stopped.door {
        Ok(result) => {
            for task_id in canceled_ids(result) {
                println!("canceled: {task_id}");
            }
            for task_id in &stopped.unended {
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
}

// ── A whole formation ─────────────────────────────────────────────────────────

/// End every member of the formation `address` names, the way the formation ends itself.
///
/// The launcher is the one signalled. It writes no record, so it is found through its members,
/// each of which records the launcher its formation channel named. `SIGTERM` to it begins the
/// teardown it already runs on its own end: it closes every member's lifeline, and each member,
/// the entry member included, appends `formation_ended` to its trace and winds down. A member
/// holding a formation lifeline, or a delegated child holding its spawner's, is never signalled
/// and its door never called: something else is already ending it, and a `SIGTERM` here would end
/// it without its `formation_ended`. A member started by hand holds no lifeline, and this command
/// is the only thing that ends it, so it is stopped exactly as `mur stop <session>` would.
///
/// Every refusal before the launcher is signalled signals nothing.
fn stop_formation(address: &str, grace: Duration) -> Result<(), CliError> {
    let formation_id = FormationId::parse(address).map_err(|error| {
        let reason = match error {
            RuntimeError::FormationIdUnreadable { reason } => reason,
            other => other.to_string(),
        };
        CliError::with_hint(
            E_RUN_022,
            format!("'{address}' is not a formation id: {reason}"),
            "name a formation by the full frm_ id in the FORMATION column of mur ps",
        )
    })?;

    let mut members = Vec::new();
    let mut pruned = 0usize;
    for record in running::list().map_err(|reason| records_unreadable(&reason))? {
        if record.formation_id.as_ref() != Some(&formation_id) {
            continue;
        }
        match running::process_state(&record) {
            ProcessState::Alive | ProcessState::Unverified(_) => members.push(record),
            ProcessState::Gone(_) => {
                running::prune(&record);
                pruned += 1;
            }
        }
    }
    if members.is_empty() {
        if pruned > 0 {
            return Err(CliError::new(
                E_RUN_022,
                format!(
                    "formation {formation_id} is not running: {pruned} member record(s) named \
                     processes that are gone, and were removed"
                ),
            ));
        }
        return Err(CliError::with_hint(
            E_RUN_022,
            format!("no member of formation {formation_id} is running on this machine"),
            "`mur ps` lists the formations running here",
        ));
    }

    let mut launchers: Vec<&ProcessIdentity> = Vec::new();
    for launcher in members
        .iter()
        .filter_map(|member| member.formation_launcher.as_ref())
    {
        if !launchers.contains(&launcher) {
            launchers.push(launcher);
        }
    }
    if launchers.len() > 1 {
        return Err(CliError::with_hint(
            E_RUN_024,
            format!(
                "{} its members record {} different launchers ({})",
                unended_formation(&formation_id),
                launchers.len(),
                launchers
                    .iter()
                    .map(|launcher| format!("pid {}", launcher.pid))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "nothing was signalled; end each member with mur stop <session>",
        ));
    }

    let launcher = match launchers.first() {
        None => LauncherEnding::NoneRecorded,
        Some(launcher) => end_the_launcher(&formation_id, launcher, grace)?,
    };

    // One at a time, in member order, each exactly as `mur stop <session>` stops it. A member
    // that could not be ended is reported and counted as still running; the rest still go.
    let by_hand: Vec<(&RunningRecord, Result<SessionStop, CliError>)> = members
        .iter()
        .filter(|member| started_by_hand(member))
        .map(|member| (member, stop_session(member, grace)))
        .collect();

    let still_running = wait_for_members(&members);

    println!("stopped:  {formation_id}");
    println!("launcher: {}", launcher.line(grace));
    for member in &members {
        let ending = if started_by_hand(member) {
            STARTED_BY_HAND_ENDING.to_string()
        } else if still_running.contains(&member.session_id.as_str()) {
            STILL_RUNNING_ENDING.to_string()
        } else {
            recorded_ending(&member.workdir.join("trace.jsonl"))
        };
        println!(
            "member:   {}  {}@{}  {ending}",
            member.session_id, member.capsule_name, member.capsule_version
        );
    }
    for (_, stopped) in &by_hand {
        println!();
        match stopped {
            Ok(stopped) => print_session_stop(stopped),
            Err(error) => println!("error: {}", error.message),
        }
    }

    if let LauncherEnding::OutlivedKill(pid) = launcher {
        return Err(CliError::new(
            E_RUN_024,
            format!(
                "{} its launcher, pid {pid}, was still running {} seconds after SIGKILL",
                unended_formation(&formation_id),
                KILL_DEADLINE.as_secs()
            ),
        ));
    }
    if let LauncherEnding::KillRefused(pid, reason) = launcher {
        return Err(CliError::new(
            E_RUN_024,
            format!(
                "{} SIGKILL to its launcher, pid {pid}, was refused: {reason}",
                unended_formation(&formation_id)
            ),
        ));
    }
    if !still_running.is_empty() {
        return Err(CliError::with_hint(
            E_RUN_024,
            format!(
                "{} {} of its members were still running {} seconds later: {}",
                unended_formation(&formation_id),
                still_running.len(),
                MEMBER_STOP_GRACE.as_secs(),
                still_running.join(", ")
            ),
            "their records are kept; `mur stop <session>` ends one",
        ));
    }
    Ok(())
}

/// The prefix every formation id starts with, and that routes an address to [`stop_formation`].
const FORMATION_ID_PREFIX: &str = "frm_";

/// The `<ending>` of a member started by hand, whose own block follows the member lines.
const STARTED_BY_HAND_ENDING: &str = "started by hand — stopped as one session below";

/// The `<ending>` of a member whose process outlived the wait.
const STILL_RUNNING_ENDING: &str = "still running";

/// The opening of every formation `E-RUN-024`, before its reason.
fn unended_formation(formation_id: &FormationId) -> String {
    format!("formation {formation_id} could not be ended:")
}

/// A member nothing else will end: it holds no formation lifeline, so no launcher closes one on
/// it, and it was not delegated, so no spawner's end reaches it. What `mur run` warns `W-RUN-007`
/// about when a formation id is set by hand.
fn started_by_hand(member: &RunningRecord) -> bool {
    !member.formation_lifeline && member.spawned_by.is_none()
}

/// What signalling the formation's launcher came to, short of a refused `SIGTERM`, which is an
/// error before anything else is done.
enum LauncherEnding {
    /// No member records a launcher.
    NoneRecorded,
    /// The recorded launcher had already exited, so the kernel had closed every lifeline.
    AlreadyExited(u32),
    /// It exited within the grace after `SIGTERM`.
    Terminated(u32),
    /// It outlived the grace and exited after `SIGKILL`.
    Killed(u32),
    /// It outlived the grace, and `SIGKILL` to it was refused, with the reason.
    KillRefused(u32, String),
    /// It was still running [`KILL_DEADLINE`] after `SIGKILL`.
    OutlivedKill(u32),
}

impl LauncherEnding {
    /// The text after `launcher: `.
    fn line(&self, grace: Duration) -> String {
        match self {
            Self::NoneRecorded => "none — no running member records one".to_string(),
            Self::AlreadyExited(pid) => {
                format!("none — pid {pid} had already exited, so every lifeline was already closed")
            }
            Self::Terminated(pid) => format!("pid {pid}, SIGTERM"),
            Self::Killed(pid) | Self::KillRefused(pid, _) | Self::OutlivedKill(pid) => {
                format!("pid {pid}, SIGKILL after {}s", grace.as_secs())
            }
        }
    }
}

/// `SIGTERM` the launcher, then `SIGKILL` it once `grace` is out — to its pid alone, never its
/// group, whose other members are the entry member and nothing this command may signal.
///
/// Both signals go through [`running::signal_term_process`] and [`running::signal_kill_process`],
/// which re-verify the pid's start time immediately before `kill(2)`, so a launcher pid inherited
/// by an unrelated process is never signalled. A refused `SIGTERM` is `E-RUN-024` with nothing
/// signalled at all.
fn end_the_launcher(
    formation_id: &FormationId,
    launcher: &ProcessIdentity,
    grace: Duration,
) -> Result<LauncherEnding, CliError> {
    let pid = launcher.pid;
    match running::signal_term_process(launcher) {
        SignalOutcome::AlreadyGone => return Ok(LauncherEnding::AlreadyExited(pid)),
        SignalOutcome::Refused(reason) => {
            return Err(CliError::with_hint(
                E_RUN_024,
                format!(
                    "{} SIGTERM to its launcher, pid {pid}, was refused: {reason}",
                    unended_formation(formation_id)
                ),
                "nothing was signalled and every member is still running; the launcher may \
                 belong to another user",
            ))
        }
        SignalOutcome::Sent => {}
    }
    if wait_for_identity_exit(launcher, grace) {
        return Ok(LauncherEnding::Terminated(pid));
    }
    match running::signal_kill_process(launcher) {
        // It exited between the grace period expiring and the signal: `SIGTERM` did the work.
        SignalOutcome::AlreadyGone => return Ok(LauncherEnding::Terminated(pid)),
        SignalOutcome::Refused(reason) => return Ok(LauncherEnding::KillRefused(pid, reason)),
        SignalOutcome::Sent => {}
    }
    if wait_for_identity_exit(launcher, KILL_DEADLINE) {
        return Ok(LauncherEnding::Killed(pid));
    }
    Ok(LauncherEnding::OutlivedKill(pid))
}

/// Wait up to [`MEMBER_STOP_GRACE`] — the grace the launcher itself gives its members — for every
/// member's process to be gone, pruning each record once it is. Returns the session ids still
/// running at the end, in member order.
///
/// Nothing here signals or calls anything: a member that holds a lifeline is ended by its closing,
/// and one started by hand was already stopped, or could not be.
fn wait_for_members(members: &[RunningRecord]) -> Vec<&str> {
    let deadline = Instant::now() + MEMBER_STOP_GRACE;
    let mut pending: Vec<&RunningRecord> = members.iter().collect();
    loop {
        pending.retain(|member| {
            let gone = matches!(running::process_state(member), ProcessState::Gone(_));
            if gone {
                running::prune(member);
            }
            !gone
        });
        if pending.is_empty() || Instant::now() >= deadline {
            return pending
                .iter()
                .map(|member| member.session_id.as_str())
                .collect();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// How a member's trace says it ended: the first of `formation_ended`, `spawner_ended`,
/// `session_end <exit_status>` it records, or `no session_end`.
///
/// `formation_ended` and `spawner_ended` name what ended the session, which is the question;
/// `session_end` only says that it ended. A missing trace, or a line that does not parse, records
/// nothing.
fn recorded_ending(trace_path: &Path) -> String {
    let events: Vec<Value> = std::fs::read_to_string(trace_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let has = |kind: &str| {
        events
            .iter()
            .any(|event| event.get("event_type").and_then(Value::as_str) == Some(kind))
    };
    if has("formation_ended") {
        return "formation_ended".to_string();
    }
    if has("spawner_ended") {
        return "spawner_ended".to_string();
    }
    events
        .iter()
        .find(|event| event.get("event_type").and_then(Value::as_str) == Some("session_end"))
        .map(|event| {
            format!(
                "session_end {}",
                event
                    .get("exit_status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )
        })
        .unwrap_or_else(|| "no session_end".to_string())
}

/// Whether the process `identity` names is gone before `grace` runs out. A zero grace checks once.
fn wait_for_identity_exit(identity: &ProcessIdentity, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if matches!(running::identity_state(identity), ProcessState::Gone(_)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL.min(grace));
    }
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

    let response =
        super::cancel::post_json(addr, &body, record.door_token.as_ref()).map_err(|e| e.message)?;
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
/// command started is not signalled. A pid whose start time cannot be read is refused rather than
/// signalled, and that refusal is `E-RUN-024` with the record kept; its reason is the one
/// [`capsule_runtime::ProcessState::Unverified`] carries, and the errno description otherwise.
fn end_the_process(record: &RunningRecord, grace: Duration) -> Result<&'static str, CliError> {
    match running::signal_term(record) {
        SignalOutcome::AlreadyGone => return Ok("none — the process had already exited"),
        SignalOutcome::Refused(reason) => {
            return Err(unendable(
                record,
                &format!("SIGTERM to pid {} was refused: {reason}", record.pid),
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
                &format!("SIGKILL to pid {} was refused: {reason}", record.pid),
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
    fn a_members_ending_names_what_ended_it_before_how_it_ended() {
        let cases: [(&[&str], &str); 5] = [
            (
                &[
                    r#"{"event_type":"formation_ended","formation_id":"frm_x"}"#,
                    r#"{"event_type":"session_end","exit_status":"terminated"}"#,
                ],
                "formation_ended",
            ),
            (
                &[
                    r#"{"event_type":"spawner_ended"}"#,
                    r#"{"event_type":"session_end","exit_status":"terminated"}"#,
                ],
                "spawner_ended",
            ),
            (
                &[
                    r#"{"event_type":"session_end","exit_status":"ok"}"#,
                    r#"{"event_type":"session_end","exit_status":"error"}"#,
                ],
                "session_end ok",
            ),
            (
                &[r#"{"event_type":"task_start","task_id":"tsk_s"}"#],
                "no session_end",
            ),
            (&[r#"{"event_type":"formation_end"#], "no session_end"),
        ];
        for (lines, ending) in cases {
            let (_dir, path) = trace_with(lines);
            assert_eq!(recorded_ending(&path), ending, "{lines:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            recorded_ending(&dir.path().join("trace.jsonl")),
            "no session_end"
        );
    }

    #[test]
    fn only_a_member_holding_no_lifeline_and_no_spawner_was_started_by_hand() {
        let mut member = RunningRecord {
            session_id: "ses_0199c4e2f1b7712a9d3e4f5061728394".to_string(),
            url: "127.0.0.1:1".to_string(),
            pid: 1,
            process_start: "42".to_string(),
            capsule_name: "coder".to_string(),
            capsule_version: "1.2.0".to_string(),
            workdir: std::path::PathBuf::from("/tmp/coder"),
            outlives_launcher: true,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            formation_id: Some(FormationId::mint()),
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
        };
        assert!(started_by_hand(&member));
        member.spawned_by = Some("ses_0199c4e2f1b7712a9d3e4f5061728390".to_string());
        assert!(!started_by_hand(&member));
        member.spawned_by = None;
        member.formation_lifeline = true;
        assert!(!started_by_hand(&member));
    }

    #[test]
    fn each_launcher_ending_has_its_own_line() {
        let grace = Duration::from_secs(10);
        assert_eq!(
            LauncherEnding::NoneRecorded.line(grace),
            "none — no running member records one"
        );
        assert_eq!(
            LauncherEnding::AlreadyExited(7).line(grace),
            "none — pid 7 had already exited, so every lifeline was already closed"
        );
        assert_eq!(LauncherEnding::Terminated(7).line(grace), "pid 7, SIGTERM");
        assert_eq!(
            LauncherEnding::Killed(7).line(Duration::ZERO),
            "pid 7, SIGKILL after 0s"
        );
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
