//! The capsules running on this machine.

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
};

use capsule_runtime::{running, FormationId, Liveness, RunningRecord};
use chrono::{DateTime, Utc};

use crate::error::CliError;
use crate::formation_trace::recorded_members;
use crate::live_address::records_unreadable;

/// Full session id: `ses_` and 32 hex characters, never abbreviated — the column exists so the
/// id can be copied into the next command.
const SESSION_WIDTH: usize = 36;
/// `name@version`.
const CAPSULE_WIDTH: usize = 24;
/// `running` or `unreachable`.
const STATUS_WIDTH: usize = 12;
/// `yes` or `no`.
const DETACHED_WIDTH: usize = 8;
/// `HH:MM:SS`, which a run past a day outgrows by its `Nd ` prefix rather than being truncated.
const UPTIME_WIDTH: usize = 9;
/// A full `frm_` id, or `-` for a session in no formation.
const FORMATION_WIDTH: usize = 36;

/// What a row says about the capsule it names, once the record has been verified.
const STATUS_RUNNING: &str = "running";
const STATUS_UNREACHABLE: &str = "unreachable";

/// List every capsule running on this machine, verifying each record before printing it.
///
/// Host-scoped, exactly like `docker ps`. A capsule deployed onto another machine writes its
/// record on *that* machine, so it is that machine's `mur ps` that lists it.
///
/// The listing is also the reaper: a record whose process is gone is unlinked on the way past, and
/// each unlink is named on stderr as `pruned: <session_id> — <reason>`, with ` (formation <id>)`
/// appended for a member. A record whose process is alive and whose door is quiet, or whose start
/// time could not be read, is kept and reported as `unreachable` — neither is evidence of a dead
/// capsule, and unlinking it would throw away the only handle to something still running.
///
/// When any listed or pruned record names a formation, the table gains a `FORMATION` column, a
/// formation's rows are kept together, and one summary line per formation follows the table. A
/// machine with no member prints exactly what it printed before formations existed.
///
/// A record directory that cannot be read fails with `E-RUN-028` and prints nothing on stdout.
/// Nothing a formation summary reads can fail the command.
pub(crate) fn run_ps() -> Result<(), CliError> {
    let records = running::list().map_err(|reason| records_unreadable(&reason))?;
    let mut rows = Vec::new();
    let mut pruned = Vec::new();
    for record in records {
        match running::verify(&record) {
            Liveness::Live => rows.push((record, STATUS_RUNNING)),
            Liveness::Unreachable(_) => rows.push((record, STATUS_UNREACHABLE)),
            Liveness::Gone(reason) => {
                running::prune(&record);
                match &record.formation_id {
                    Some(formation) => eprintln!(
                        "pruned: {} — {reason} (formation {formation})",
                        record.session_id
                    ),
                    None => eprintln!("pruned: {} — {reason}", record.session_id),
                }
                pruned.push(record);
            }
        }
    }

    let any_member = rows
        .iter()
        .map(|(record, _)| record)
        .chain(&pruned)
        .any(|record| record.formation_id.is_some());
    if !any_member {
        print_table(&rows, false);
        return Ok(());
    }

    let rows = grouped(rows);
    print_table(&rows, true);
    println!();
    for formation in formations_in_order(&rows, &pruned) {
        println!("{}", summary_line(&formation, &rows, &pruned));
    }
    Ok(())
}

/// The table, or `no running capsules` when no row survived. `with_formation` adds the
/// `FORMATION` column between `UPTIME` and `URL`.
fn print_table(rows: &[(RunningRecord, &str)], with_formation: bool) {
    // A machine with no record directory and a machine with an empty one are the same fact, and
    // are reported in the same words.
    if rows.is_empty() {
        println!("no running capsules");
        return;
    }

    let formation_header = if with_formation {
        format!("{:<FORMATION_WIDTH$}  ", "FORMATION")
    } else {
        String::new()
    };
    println!(
        "{:<SESSION_WIDTH$}  {:<CAPSULE_WIDTH$}  {:<STATUS_WIDTH$}  {:<DETACHED_WIDTH$}  {:<UPTIME_WIDTH$}  {formation_header}URL",
        "SESSION", "CAPSULE", "STATUS", "DETACHED", "UPTIME"
    );
    for (record, status) in rows {
        let formation = if with_formation {
            let id = record
                .formation_id
                .as_ref()
                .map_or("-", FormationId::as_str);
            format!("{id:<FORMATION_WIDTH$}  ")
        } else {
            String::new()
        };
        println!(
            "{:<SESSION_WIDTH$}  {:<CAPSULE_WIDTH$}  {:<STATUS_WIDTH$}  {:<DETACHED_WIDTH$}  {:<UPTIME_WIDTH$}  {formation}{}",
            record.session_id,
            format!("{}@{}", record.capsule_name, record.capsule_version),
            status,
            if record.outlives_launcher { "yes" } else { "no" },
            uptime(record),
            record.url,
        );
    }
}

/// `rows`, in session id descending order, regrouped so each formation's rows are contiguous.
///
/// A session in no formation is a group of one. Groups are ordered by their newest member and
/// rows within a group stay newest first, so the first row is still the newest session — what
/// `@1` names — while past it a row's position and its `@N` may differ.
fn grouped(rows: Vec<(RunningRecord, &str)>) -> Vec<(RunningRecord, &str)> {
    let mut groups: Vec<Vec<(RunningRecord, &str)>> = Vec::new();
    for row in rows {
        let joins = row.0.formation_id.as_ref().and_then(|formation| {
            groups
                .iter()
                .position(|group| group[0].0.formation_id.as_ref() == Some(formation))
        });
        match joins {
            Some(index) => groups[index].push(row),
            None => groups.push(vec![row]),
        }
    }
    groups.into_iter().flatten().collect()
}

/// Every formation a listed or pruned record names: the listed ones in row order, then those
/// known only from pruned records, newest member first.
fn formations_in_order(
    rows: &[(RunningRecord, &str)],
    pruned: &[RunningRecord],
) -> Vec<FormationId> {
    let mut formations: Vec<FormationId> = Vec::new();
    for record in rows.iter().map(|(record, _)| record).chain(pruned) {
        if let Some(formation) = &record.formation_id {
            if !formations.contains(formation) {
                formations.push(formation.clone());
            }
        }
    }
    formations
}

/// `formation <id>: <L> listed (<statuses>)[, <P> pruned now][; <not-listed clause>][; <U> session
/// root(s) could not be read]`.
///
/// Never a claim that the formation is whole. It counts what this read listed and pruned, then
/// what the members' traces under their session roots record beyond that: members that ended,
/// and members with no `session_end` and no record — killed, or never recorded.
fn summary_line(
    formation: &FormationId,
    rows: &[(RunningRecord, &str)],
    pruned: &[RunningRecord],
) -> String {
    let in_formation = |record: &&RunningRecord| record.formation_id.as_ref() == Some(formation);
    let listed: Vec<&str> = rows
        .iter()
        .filter(|(record, _)| in_formation(&record))
        .map(|(_, status)| *status)
        .collect();
    let pruned_here: Vec<&RunningRecord> = pruned.iter().filter(in_formation).collect();

    let mut line = format!("formation {formation}: {} listed", listed.len());
    if !listed.is_empty() {
        let statuses = counted(&[
            (count_of(&listed, STATUS_RUNNING), STATUS_RUNNING),
            (count_of(&listed, STATUS_UNREACHABLE), STATUS_UNREACHABLE),
        ]);
        line.push_str(&format!(" ({statuses})"));
    }
    if !pruned_here.is_empty() {
        line.push_str(&format!(", {} pruned now", pruned_here.len()));
    }

    // A record's `workdir` is its session directory, so its parent is the root its siblings are in.
    let roots: BTreeSet<PathBuf> = rows
        .iter()
        .map(|(record, _)| record)
        .filter(in_formation)
        .chain(pruned_here.iter().copied())
        .filter_map(|record| record.workdir.parent().map(PathBuf::from))
        .collect();
    // Every session this read had a record for, so a trace is never counted beside its own row.
    let recorded: HashSet<&str> = rows
        .iter()
        .map(|(record, _)| record)
        .chain(pruned)
        .map(|record| record.session_id.as_str())
        .collect();
    let mut seen = HashSet::new();
    let (mut ended, mut no_record, mut searched, mut unreadable) = (0, 0, 0, 0);
    for root in &roots {
        let Ok(found) = recorded_members(root, formation) else {
            unreadable += 1;
            continue;
        };
        searched += 1;
        for member in found.members {
            if recorded.contains(member.session_id.as_str()) || !seen.insert(member.session_id) {
                continue;
            }
            if member.exit_status.is_some() {
                ended += 1;
            } else {
                no_record += 1;
            }
        }
    }

    if ended + no_record > 0 {
        let not_listed = counted(&[(ended, "ended"), (no_record, "with no record")]);
        line.push_str(&format!("; not listed: {not_listed}"));
    } else {
        line.push_str(&format!(
            "; no other member found in {searched} session {}",
            if searched == 1 { "root" } else { "roots" }
        ));
    }
    if unreadable > 0 {
        line.push_str(&format!(
            "; {unreadable} session {} could not be read",
            if unreadable == 1 { "root" } else { "roots" }
        ));
    }
    line
}

fn count_of(statuses: &[&str], status: &str) -> usize {
    statuses.iter().filter(|listed| **listed == status).count()
}

/// `<n> <what>` for every non-zero count, joined by `, `.
fn counted(counts: &[(usize, &str)]) -> String {
    counts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// How long the session has been up, from its recorded start to now.
///
/// A `started_at` that does not parse, or a clock that has moved backwards since it was written,
/// renders as `-`: the row still names a capsule that is verifiably running, and a duration is
/// the one thing about it that could not be read.
fn uptime(record: &RunningRecord) -> String {
    let Ok(started) = DateTime::parse_from_rfc3339(&record.started_at) else {
        return "-".to_string();
    };
    let Ok(seconds) = u64::try_from(
        Utc::now()
            .signed_duration_since(started.with_timezone(&Utc))
            .num_seconds(),
    ) else {
        return "-".to_string();
    };
    format_uptime(seconds)
}

/// `HH:MM:SS`, with a `Nd ` prefix once the session has been up longer than a day.
fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let clock = format!(
        "{:02}:{:02}:{:02}",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    );
    if days == 0 {
        clock
    } else {
        format!("{days}d {clock}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session_id: &str, formation: Option<&FormationId>) -> (RunningRecord, &'static str) {
        let record = RunningRecord {
            session_id: session_id.to_string(),
            url: "127.0.0.1:1".to_string(),
            pid: 1,
            process_start: String::new(),
            capsule_name: "c".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/nonexistent/root").join(session_id),
            outlives_launcher: false,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            formation_id: formation.cloned(),
        };
        (record, STATUS_RUNNING)
    }

    /// A formation's rows are contiguous, groups go newest member first, and the newest session
    /// overall stays the first row.
    #[test]
    fn rows_group_by_formation_and_keep_the_newest_first() {
        let a = FormationId::mint();
        let b = FormationId::mint();
        let rows = vec![
            row("ses_9", Some(&a)),
            row("ses_8", None),
            row("ses_7", Some(&b)),
            row("ses_6", Some(&a)),
            row("ses_5", None),
            row("ses_4", Some(&b)),
        ];
        let order: Vec<String> = grouped(rows)
            .into_iter()
            .map(|(record, _)| record.session_id)
            .collect();
        assert_eq!(
            order,
            ["ses_9", "ses_6", "ses_8", "ses_7", "ses_4", "ses_5"]
        );
    }

    /// A formation known only from a pruned record still gets its line, after the listed ones.
    #[test]
    fn a_formation_seen_only_pruned_is_summarized_last() {
        let listed = FormationId::mint();
        let gone = FormationId::mint();
        let rows = vec![row("ses_2", Some(&listed))];
        let pruned = vec![row("ses_3", Some(&gone)).0, row("ses_1", Some(&listed)).0];
        assert_eq!(
            formations_in_order(&rows, &pruned),
            vec![listed.clone(), gone.clone()]
        );
        assert_eq!(
            summary_line(&gone, &rows, &pruned),
            format!(
                "formation {gone}: 0 listed, 1 pruned now; no other member found in 1 session root"
            )
        );
        assert_eq!(
            summary_line(&listed, &rows, &pruned),
            format!(
                "formation {listed}: 1 listed (1 running), 1 pruned now; no other member found in 1 session root"
            )
        );
    }

    #[test]
    fn an_uptime_under_a_day_is_a_clock() {
        assert_eq!(format_uptime(0), "00:00:00");
        assert_eq!(format_uptime(59), "00:00:59");
        assert_eq!(format_uptime(3661), "01:01:01");
        assert_eq!(format_uptime(86_399), "23:59:59");
    }

    /// Past a day the clock restarts and the day count is prefixed, so `23:59:59` never means
    /// two different elapsed times.
    #[test]
    fn an_uptime_past_a_day_carries_the_day_count() {
        assert_eq!(format_uptime(86_400), "1d 00:00:00");
        assert_eq!(format_uptime(90_061), "1d 01:01:01");
        assert_eq!(format_uptime(864_000), "10d 00:00:00");
    }

    /// The clock fits the column; the day prefix is allowed to push the URL right rather than
    /// lose a digit.
    #[test]
    fn the_clock_form_fits_its_column() {
        assert!(format_uptime(86_399).len() <= UPTIME_WIDTH);
    }
}
