//! What a resumed launch reports about work the session it resumes never accounted for.
//!
//! A demoted shell command leaves a `shell_detached` line, flushed at the demotion instant. Every
//! ordinary way of ending a session then writes a matching `shell_completed` or `shell_abandoned`:
//! the teardown sweep in [`crate::runtime`] runs after `session_end` on every clean exit. A
//! `shell_detached` with neither therefore means the sweep never ran, which is what happens when
//! the runtime is killed outright.
//!
//! The job is accounting. No pid was recorded, no liveness is checked, no orphan is adopted and no
//! result is recovered, because none can be — see [`crate::detached::LostWork`] for why a killed
//! runtime leaves no exit code even for a command that ran to a clean exit.
//!
//! The reader lives beside the writer that owns the format rather than in `murmur-cli`: the
//! dependency runs cli → runtime and not back, and a delegated child is launched from a parent's
//! runtime rather than from `mur run`, so a reconciler in the CLI would be on a path the
//! delegation case could not use.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::{
    detached::{LostReport, LostWork},
    origin::{TaskOrigin, TaskProvenance, TrustClass},
    trace::PriorSessionTraceAppender,
};

/// What one prior session's `trace.jsonl` says about its own demoted commands.
struct PriorSessionScan {
    /// The `session_start` line's `session_id`: the session that started the work, which every
    /// marker appended back names. `None` when the trace records no `session_start`.
    session_id: Option<String>,
    /// The `session_start` line's `event_id`, which every marker appended back hangs off.
    session_event_id: Option<String>,
    /// Demoted and accounted for by nothing, oldest work id first.
    unaccounted: Vec<LostWork>,
    /// The lowest trust across [`Self::unaccounted`], joined through each command's `task_id`.
    trust: TrustClass,
}

/// Report the unaccounted commands of the session whose `trace.jsonl` is in `trace_dir`, marking
/// each as lost in that same file, or `None` when there is nothing to report.
///
/// `trace_dir` is [`crate::ResumeRequest::trace_dir`]: the prior session's records directory, or
/// wherever its trace was copied. The session that started the work is the one the trace's own
/// `session_start` names, whatever the directory is called.
///
/// Fails open at every step but one: a trace that is missing, cannot be read or records no
/// `session_start`, and a `shell_lost` line that cannot be appended, each leave the launch running
/// with a line on stderr, because refusing to launch over an accounting record would cost more
/// than the record is worth. The exception is a work id whose marker did not land: it is dropped
/// from the report rather than reported unmarked, since an unmarked work id would be reported
/// again by every later resume of the same trace.
pub(crate) async fn reconcile_prior_session(
    trace_dir: &crate::workdir_fs::WorkdirRoot,
    this_session_id: &str,
    context_id: &str,
) -> Option<LostReport> {
    let trace_file = crate::trace::TRACE_FILE_NAME;
    let trace_path = trace_dir.display_path(trace_file);
    let contents = match trace_dir.read_to_string(trace_file) {
        Ok(Some(contents)) => contents,
        Ok(None) => {
            crate::runtime_err!(
                "[capsule-runtime] could not read {trace_path} (it does not exist); background \
                 work the resumed session left unaccounted is not reported"
            );
            return None;
        }
        Err(error) => {
            crate::runtime_err!(
                "[capsule-runtime] could not read {error}; background work the resumed session \
                 left unaccounted is not reported"
            );
            return None;
        }
    };

    let scan = scan_prior_trace(&contents);
    let Some(started_in_session) = scan.session_id else {
        crate::runtime_err!(
            "[capsule-runtime] {trace_path} records no session_start, so the session that \
             started its background work is unknown; background work it left unaccounted is not \
             reported"
        );
        return None;
    };
    if scan.unaccounted.is_empty() {
        return None;
    }

    // Minted here rather than at the enqueue: every `shell_lost` line names it, so it has to
    // exist before the first marker is written.
    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let mut appender = match PriorSessionTraceAppender::open(
        trace_dir,
        trace_file,
        started_in_session.clone(),
        scan.session_event_id,
    )
    .await
    {
        Ok(appender) => appender,
        Err(error) => {
            crate::runtime_err!(
                "[capsule-runtime] could not append to {trace_path} ({error}); the {} background \
                 command(s) session {started_in_session} left unaccounted are not reported",
                scan.unaccounted.len()
            );
            return None;
        }
    };

    let mut lost = Vec::new();
    for work in scan.unaccounted {
        if let Err(error) = appender
            .write_shell_lost(&work, this_session_id, &task_id)
            .await
        {
            crate::runtime_err!(
                "[capsule-runtime] could not mark background command {} of session \
                 {started_in_session} as lost in {trace_path} ({error}); it is not reported",
                work.work_id
            );
            continue;
        }
        lost.push(work);
    }
    if lost.is_empty() {
        return None;
    }

    Some(LostReport {
        started_in_session,
        lost,
        context_id: context_id.to_string(),
        provenance: TaskProvenance::derive(TaskOrigin::Completion, Some(scan.trust)),
        task_id,
    })
}

/// Read the file as `event_type`-keyed JSON values, without rebuilding a typed event enum.
///
/// A line that does not parse is skipped rather than aborting: a torn tail is the expected shape
/// of a file whose writer was killed, and the records above it are still the account of what the
/// session did.
fn scan_prior_trace(contents: &str) -> PriorSessionScan {
    let mut session_id = None;
    let mut session_event_id = None;
    // Keyed on the work id, which is a UUID v7, so iteration order is demotion order.
    let mut detached: BTreeMap<String, (LostWork, Option<String>)> = BTreeMap::new();
    let mut accounted: BTreeSet<String> = BTreeSet::new();
    let mut task_trust: BTreeMap<String, TrustClass> = BTreeMap::new();

    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record["event_type"].as_str().unwrap_or_default() {
            // The first one names the session, as the CLI's resolution of `--resume` reads it.
            "session_start" if session_id.is_none() => {
                session_id = record["session_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                session_event_id = record["event_id"].as_str().map(str::to_string);
            }
            "task_start" => {
                if let (Some(task_id), Some(trust)) = (
                    record["task_id"].as_str(),
                    record["trust"].as_str().and_then(TrustClass::parse),
                ) {
                    task_trust.insert(task_id.to_string(), trust);
                }
            }
            "shell_detached" => {
                let Some(id) = record["work_id"].as_str() else {
                    continue;
                };
                detached.insert(
                    id.to_string(),
                    (
                        LostWork {
                            work_id: id.to_string(),
                            binary: record["binary"].as_str().unwrap_or_default().to_string(),
                            command: record["command"].as_str().unwrap_or_default().to_string(),
                            detached_at_ms: record["timestamp"].as_u64().unwrap_or_default(),
                        },
                        record["task_id"].as_str().map(str::to_string),
                    ),
                );
            }
            // The three ways a demoted command is already accounted for: it finished and was
            // reported, the session's teardown sweep gave up on it, or an earlier resume marked
            // it lost.
            "shell_completed" | "shell_abandoned" | "shell_lost" => {
                if let Some(id) = record["work_id"].as_str() {
                    accounted.insert(id.to_string());
                }
            }
            _ => {}
        }
    }

    let mut unaccounted = Vec::new();
    let mut trust = TrustClass::Trusted;
    for (work_id, (work, task_id)) in detached {
        if accounted.contains(&work_id) {
            continue;
        }
        // A join that finds nothing yields untrusted: an absent claim is not a trusted one, the
        // same rule [`TaskProvenance::derive`] applies to an absent inherited class.
        let work_trust = task_id
            .and_then(|task_id| task_trust.get(&task_id).copied())
            .unwrap_or(TrustClass::Untrusted);
        if work_trust == TrustClass::Untrusted {
            trust = TrustClass::Untrusted;
        }
        unaccounted.push(work);
    }

    PriorSessionScan {
        session_id,
        session_event_id,
        unaccounted,
        trust,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use std::path::PathBuf;

    /// A trace holding one task and one demoted command, with `extra` lines appended verbatim.
    fn trace_with(extra: &str) -> String {
        let mut lines = String::from(
            r#"{"event_type":"session_start","event_id":"evt_session","session_id":"ses_prior","timestamp":1}
{"event_type":"task_start","event_id":"evt_task","session_id":"ses_prior","timestamp":2,"task_id":"tsk_1","context_id":"ctx_1","source":"task_md","origin":"user","trust":"trusted","lane":"user"}
{"event_type":"shell_detached","event_id":"evt_detached","session_id":"ses_prior","timestamp":1750,"turn":1,"task_id":"tsk_1","work_id":"wrk_one","binary":"/usr/bin/bash","command":"sleep 60","grace_ms":1000}
"#,
        );
        lines.push_str(extra);
        lines
    }

    /// Write `trace` as the `trace.jsonl` of a directory named `name`, which need not be a session
    /// id, and return that directory.
    fn trace_dir_with(name: &str, trace: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trace.jsonl"), trace).unwrap();
        (root, dir)
    }

    fn reconcile(dir: &Path, this_session_id: &str) -> Option<LostReport> {
        block_on(reconcile_prior_session(
            &crate::workdir_fs::WorkdirRoot::open(dir).unwrap(),
            this_session_id,
            "ctx_1",
        ))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn lost_lines(dir: &Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("trace.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["event_type"] == "shell_lost")
            .collect()
    }

    /// The whole claim: an unmatched `shell_detached` becomes one report and one marker, and the
    /// marker names both the session that found it and the task that reports it.
    #[test]
    fn an_unmatched_demotion_is_reported_once_and_marked_in_the_session_that_started_it() {
        let (_root, root) = trace_dir_with("bench-attempt-1", &trace_with(""));

        let report = reconcile(&root, "ses_resumed").expect("an unaccounted command is reported");

        assert_eq!(report.started_in_session, "ses_prior");
        assert_eq!(report.lost.len(), 1);
        assert_eq!(report.lost[0].work_id, "wrk_one");
        assert_eq!(report.lost[0].binary, "/usr/bin/bash");
        assert_eq!(report.lost[0].command, "sleep 60");
        assert_eq!(report.lost[0].detached_at_ms, 1750);
        assert_eq!(report.provenance.origin(), TaskOrigin::Completion);
        assert_eq!(report.provenance.trust(), TrustClass::Trusted);

        let lines = lost_lines(&root);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["work_id"], "wrk_one");
        assert_eq!(lines[0]["session_id"], "ses_prior");
        assert_eq!(lines[0]["parent_id"], "evt_session");
        assert_eq!(lines[0]["reconciled_by_session"], "ses_resumed");
        assert_eq!(lines[0]["reconciled_task_id"], report.task_id);
        assert_eq!(lines[0]["detached_at_ms"], 1750);
        for absent in [
            "exit_code",
            "status",
            "duration_ms",
            "output_path",
            "output_bytes",
        ] {
            assert!(
                lines[0].get(absent).is_none(),
                "a lost command has no {absent}: {}",
                lines[0]
            );
        }

        let message = report.message_text();
        assert!(
            !message.starts_with("Background shell command finished."),
            "the loss report must not open like a completion: {message}"
        );
        for named in ["work_id: wrk_one", "detached_at_ms: 1750"] {
            assert!(
                message.lines().any(|line| line == named),
                "the report names {named}: {message}"
            );
        }
        for absent in ["binary:", "command:", "/usr/bin/bash", "sleep 60"] {
            assert!(
                !message.contains(absent),
                "the work id names the command, so {absent} is not repeated: {message}"
            );
        }
    }

    /// The marker clears: a second pass over the same file finds the work id accounted for.
    #[test]
    fn a_marked_work_id_is_not_reported_again() {
        let (_root, root) = trace_dir_with("bench-attempt-1", &trace_with(""));

        assert!(reconcile(&root, "ses_resumed").is_some());
        assert!(
            reconcile(&root, "ses_third").is_none(),
            "the shell_lost line accounts for the work id"
        );
        assert_eq!(
            lost_lines(&root).len(),
            1,
            "a second resume adds no second marker"
        );
    }

    /// A session that ended cleanly accounted for its own work, whichever way it did so.
    #[test]
    fn a_command_already_accounted_for_is_not_reported() {
        for accounting in [
            r#"{"event_type":"shell_abandoned","event_id":"evt_a","session_id":"ses_prior","timestamp":9,"work_id":"wrk_one","binary":"/usr/bin/bash","command":"sleep 60","running_ms":30000}"#,
            r#"{"event_type":"shell_completed","event_id":"evt_c","session_id":"ses_prior","timestamp":9,"work_id":"wrk_one","binary":"/usr/bin/bash","command":"sleep 60","exit_code":0,"duration_ms":1,"output_path":"/home/test/.murmur/records/work/ses_prior/logs/wrk_one.log","output_bytes":1,"status":"ok","completion_task_id":"tsk_2"}"#,
        ] {
            let (_root, root) =
                trace_dir_with("bench-attempt-1", &trace_with(&format!("{accounting}\n")));
            assert!(
                reconcile(&root, "ses_resumed").is_none(),
                "an accounted command must not be reported again"
            );
        }
    }

    /// A file whose writer was killed mid-line still reports everything above the tear, and the
    /// marker it appends is a line of its own rather than a splice onto the unterminated one.
    #[test]
    fn a_torn_tail_is_skipped_and_the_records_above_it_still_report() {
        let (_root, root) = trace_dir_with(
            "bench-attempt-1",
            &trace_with("not json at all\n{\"event_type\":\"shell_comple"),
        );
        let report = reconcile(&root, "ses_resumed")
            .expect("the complete records above the tear still report");
        assert_eq!(report.lost.len(), 1);
        assert_eq!(lost_lines(&root).len(), 1);
    }

    /// Nothing to read is nothing to report, and never a refused launch.
    #[test]
    fn a_missing_trace_reports_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(reconcile(root.path(), "ses_resumed").is_none());
        assert!(
            !root.path().join("trace.jsonl").exists(),
            "nothing is created for a trace that was never there"
        );
    }

    /// A trace that names no session cannot say whose work it holds: nothing is reported and
    /// nothing is appended, whatever it demoted.
    #[test]
    fn a_trace_with_no_session_start_reports_nothing_and_marks_nothing() {
        let without_start: String = trace_with("")
            .lines()
            .filter(|line| !line.contains("\"session_start\""))
            .map(|line| format!("{line}\n"))
            .collect();
        let (_root, root) = trace_dir_with("bench-attempt-1", &without_start);
        assert!(reconcile(&root, "ses_resumed").is_none());
        assert!(lost_lines(&root).is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("trace.jsonl")).unwrap(),
            without_start,
            "the trace is left as it was"
        );
    }

    /// The session that started the work is the one the trace's own `session_start` names, not the
    /// directory the trace sits in.
    #[test]
    fn the_report_names_the_session_the_trace_records() {
        let (_root, root) = trace_dir_with("ses_someone_else", &trace_with(""));
        let report = reconcile(&root, "ses_resumed").unwrap();
        assert_eq!(report.started_in_session, "ses_prior");
        assert_eq!(lost_lines(&root)[0]["session_id"], "ses_prior");
    }

    /// Untrust survives the round trip through a dead session: the report inherits the class of
    /// the task that started the command, and an unjoinable command is untrusted.
    #[test]
    fn the_report_carries_the_lowest_trust_across_the_work_it_names() {
        let untrusted = r#"{"event_type":"task_start","event_id":"evt_task2","session_id":"ses_prior","timestamp":3,"task_id":"tsk_2","context_id":"ctx_1","source":"a2a","origin":"event","trust":"untrusted","lane":"peer"}
{"event_type":"shell_detached","event_id":"evt_detached2","session_id":"ses_prior","timestamp":1900,"turn":1,"task_id":"tsk_2","work_id":"wrk_two","binary":"/usr/bin/bash","command":"sleep 90","grace_ms":1000}
"#;
        let (_root, root) = trace_dir_with("bench-attempt-1", &trace_with(untrusted));
        let report = reconcile(&root, "ses_resumed").unwrap();
        assert_eq!(
            report.lost.len(),
            2,
            "both commands are reported as one task"
        );
        assert_eq!(report.provenance.trust(), TrustClass::Untrusted);

        let orphaned = r#"{"event_type":"shell_detached","event_id":"evt_detached3","session_id":"ses_prior","timestamp":1950,"turn":1,"task_id":"tsk_gone","work_id":"wrk_three","binary":"/usr/bin/bash","command":"sleep 90","grace_ms":1000}
"#;
        let (_root, root) = trace_dir_with("bench-attempt-1", &trace_with(orphaned));
        let report = reconcile(&root, "ses_resumed").unwrap();
        assert_eq!(report.provenance.trust(), TrustClass::Untrusted);
    }
}
