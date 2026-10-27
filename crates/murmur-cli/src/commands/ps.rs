//! The capsules running on this machine.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
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

/// What follows `skipped: <path>` for a record an older `mur` wrote.
const OLDER_MUR_SKIPPED: &str = "a record an older mur wrote, which this mur cannot read; it is \
     left in place, so stop that capsule with the mur that started it, then delete this file once \
     that capsule is gone";

/// What a row says about the capsule it names, once the record has been verified.
const STATUS_RUNNING: &str = "running";
const STATUS_UNREACHABLE: &str = "unreachable";

/// Why a member of a listed formation has no entry in `capsules`, read from its trace.
const REASON_ENDED: &str = "ended";
const REASON_GONE_WITHOUT_ENDING: &str = "gone_without_ending";

/// List every capsule running on this machine, verifying each record before printing it.
///
/// Host-scoped, exactly like `docker ps`. A capsule deployed onto another machine writes its
/// record on *that* machine, so it is that machine's `mur ps` that lists it.
///
/// The listing is also the reaper: a record whose process is gone is unlinked on the way past, as
/// is a record file that cannot be read. A record whose process is alive and whose door is quiet,
/// or whose start time could not be read, is kept and reported as `unreachable` — neither is
/// evidence of a dead capsule, and unlinking it would throw away the only handle to something
/// still running.
///
/// A record an older `mur` wrote is neither listed nor removed: its capsule may still be running,
/// and only the `mur` that started it can reach it. Each one is named on stderr first, in both
/// modes, as `skipped: <path> — ` followed by [`OLDER_MUR_SKIPPED`], and none is counted as
/// pruned.
///
/// What was unlinked is stated on stderr, before the table. By default that is one line counting
/// every removal, [`pruned_summary`], and nothing when there was none. With `verbose` it is one
/// line per removal: `pruned: <session_id> — <reason>`, with ` (formation <id>)` appended for a
/// member, and `pruned: <path> — not a readable record` for an unreadable file.
///
/// When a record that keys formations names one, the table gains a `FORMATION` column, a
/// formation's rows are kept together, and one summary line per keyed formation follows the
/// table. By default only listed records key formations; with `verbose`, pruned records do too,
/// so a formation known only from pruned records is summarised last. With no keyed member there
/// is neither the column nor a summary line.
///
/// A record directory that cannot be read fails with `E-RUN-028` and prints nothing on stdout.
/// Nothing a formation summary reads can fail the command.
///
/// With `json` the listing is [`run_ps_json`]'s instead, which prunes nothing.
pub(crate) fn run_ps(verbose: bool, json: bool) -> Result<(), CliError> {
    if json {
        return run_ps_json();
    }
    let listing = running::list_reporting().map_err(|reason| records_unreadable(&reason))?;
    for path in &listing.older_mur {
        capsule_runtime::report_eprintln!("skipped: {} — {OLDER_MUR_SKIPPED}", path.display());
    }
    let mut rows = Vec::new();
    let mut pruned = Vec::new();
    for record in listing.records {
        let liveness = running::verify(&record);
        if liveness.is_running() {
            rows.push((record, row_status(&liveness)));
            continue;
        }
        running::prune(&record);
        if verbose {
            let reason = liveness.reason();
            match &record.formation_id {
                Some(formation) => capsule_runtime::report_eprintln!(
                    "pruned: {} — {reason} (formation {formation})",
                    record.session_id
                ),
                None => {
                    capsule_runtime::report_eprintln!("pruned: {} — {reason}", record.session_id)
                }
            }
        }
        pruned.push(record);
    }
    if verbose {
        for path in &listing.removed_unreadable {
            capsule_runtime::report_eprintln!("pruned: {} — not a readable record", path.display());
        }
    } else if let Some(line) = pruned_summary(pruned.len() + listing.removed_unreadable.len()) {
        capsule_runtime::report_eprintln!("{line}");
    }

    // By default pruned records are only counted, so a formation known only from them is not
    // summarised; a listed formation's summary still counts its pruned members.
    let keyed: &[RunningRecord] = if verbose { &pruned } else { &[] };
    let any_member = rows
        .iter()
        .map(|(record, _)| record)
        .chain(keyed)
        .any(|record| record.formation_id.is_some());
    if !any_member {
        print_table(&rows, false);
        return Ok(());
    }

    let rows = grouped(rows);
    print_table(&rows, true);
    capsule_runtime::report_println!();
    for formation in formations_in_order(&rows, keyed) {
        capsule_runtime::report_println!("{}", summary_line(&formation, &rows, &pruned));
    }
    Ok(())
}

/// `mur ps --json`: a stable interface. Keys keep their names, types and meanings and serialize
/// in declaration order; a key may be added, never removed or renamed.
#[derive(Debug, serde::Serialize)]
struct CapsuleListing {
    /// One entry per record `mur ps` would print a row for, most recent session first.
    capsules: Vec<ListedCapsule>,
    /// One entry per formation an entry of `capsules` names, in the order of each formation's
    /// first entry there. Empty when none names one.
    formations: Vec<ListedFormation>,
}

/// What this read knows of one formation beyond its entries in `capsules`: the formation's
/// [`FormationCensus`], as plain `mur ps` summarises it in one line.
///
/// Never a claim that the formation is complete, and there is no member total: the roster is not
/// read, so a member that left neither a running record nor a trace under a searched root is
/// counted nowhere.
#[derive(Debug, serde::Serialize)]
struct ListedFormation {
    formation_id: String,
    /// The entry member's roster name: from the record in this read that says it is the entry
    /// member, a listed one first, else a gone one kept on disk. `null` when no record in this
    /// read says so.
    entry_member: Option<String>,
    /// How many entries of `capsules` carry this formation id.
    listed: usize,
    /// How many of `not_listed` have the reason [`REASON_ENDED`].
    ended: usize,
    /// How many of `not_listed` have the reason [`REASON_GONE_WITHOUT_ENDING`].
    gone_without_ending: usize,
    /// Every member this read knows of that has no entry in `capsules`, by session id descending.
    not_listed: Vec<NotListedMember>,
    /// Session roots that were listed.
    searched_roots: usize,
    /// Session roots that exist and could not be listed.
    unreadable_roots: usize,
}

/// A member of a listed formation with no entry in `capsules`.
#[derive(Debug, serde::Serialize)]
struct NotListedMember {
    session_id: String,
    /// The roster name: the running record's when one is kept and names one, else
    /// `session_start`'s. `null` when neither names one, as for a member's delegated child.
    formation_member: Option<String>,
    /// [`REASON_ENDED`] when the member's trace holds a `session_end`, else
    /// [`REASON_GONE_WITHOUT_ENDING`] — killed outright, its trace unreadable, or never recorded.
    reason: &'static str,
    /// `true` for a running record left on disk whose process is gone — the record plain `mur ps`
    /// prunes and counts as `pruned now`.
    record_kept: bool,
}

/// One running record, with the status its row would carry. The record's door token,
/// credentials and formation bookkeeping are never part of it.
#[derive(Debug, serde::Serialize)]
struct ListedCapsule {
    session_id: String,
    /// [`STATUS_RUNNING`] or [`STATUS_UNREACHABLE`], from [`running::verify`].
    status: &'static str,
    /// The door's `host:port`; `null` for a script capsule, which binds no door.
    url: Option<String>,
    pid: u32,
    process_start: String,
    capsule_name: String,
    capsule_version: String,
    workdir: String,
    records_dir: String,
    outlives_launcher: bool,
    started_at: String,
    formation_id: Option<String>,
    formation_member: Option<String>,
    /// `true` for the entry member of a formation, `false` for every other session.
    formation_entry: bool,
}

/// `mur ps --json`: the rows `mur ps` would print, and the formations they name, as one
/// [`CapsuleListing`].
///
/// Each record is verified by [`running::verify`] exactly as the table's rows are, and a `Gone`
/// one is left out of `capsules` and counted in its formation's `not_listed`. Nothing is created,
/// re-moded, unlinked or signalled: the directory is read with [`running::scan`], so a missing
/// `~/.murmur/running` stays missing, and a record that is gone or unreadable stays where it is.
/// A record an older `mur` wrote is named on stderr as the table names it. A directory that cannot
/// be listed, or a `HOME` it cannot be located from, fails with `E-RUN-028` and prints nothing on
/// stdout. Nothing a formation's census reads can fail it.
fn run_ps_json() -> Result<(), CliError> {
    let scan = running::running_dir_location()
        .and_then(|dir| running::scan(&dir))
        .map_err(|reason| records_unreadable(&reason))?;
    for path in &scan.older_mur {
        capsule_runtime::report_eprintln!("skipped: {} — {OLDER_MUR_SKIPPED}", path.display());
    }
    let mut rows = Vec::new();
    let mut gone = Vec::new();
    for record in scan.records {
        let liveness = running::verify(&record);
        if liveness.is_running() {
            rows.push((record, row_status(&liveness)));
        } else {
            gone.push(record);
        }
    }
    let formations = listed_formations(&rows, &gone);
    let capsules = rows
        .into_iter()
        .map(|(record, status)| listed_capsule(record, status))
        .collect();
    capsule_runtime::report_println!(
        "{}",
        serde_json::to_string_pretty(&CapsuleListing {
            capsules,
            formations
        })
        .expect("a listing of strings, integers and booleans always serializes")
    );
    Ok(())
}

/// `record`'s entry in `capsules`, with the status its row carries.
fn listed_capsule(record: RunningRecord, status: &'static str) -> ListedCapsule {
    ListedCapsule {
        status,
        url: record.url,
        pid: record.pid,
        process_start: record.process_start,
        capsule_name: record.capsule_name,
        capsule_version: record.capsule_version,
        workdir: record.workdir.display().to_string(),
        records_dir: record.records_dir.display().to_string(),
        outlives_launcher: record.outlives_launcher,
        started_at: record.started_at,
        formation_id: record
            .formation_id
            .as_ref()
            .map(|id| id.as_str().to_string()),
        formation_member: record.formation_member,
        formation_entry: record.formation_entry,
        session_id: record.session_id,
    }
}

/// The status a running record's row carries: [`STATUS_RUNNING`] when its door answered for it,
/// or, for a script capsule with no door, when its process is the one that wrote the record; else
/// [`STATUS_UNREACHABLE`].
fn row_status(liveness: &Liveness) -> &'static str {
    if *liveness == Liveness::Live {
        STATUS_RUNNING
    } else {
        STATUS_UNREACHABLE
    }
}

/// The one stderr line a default `mur ps` prints for `removed` records, or `None` for none.
fn pruned_summary(removed: usize) -> Option<String> {
    match removed {
        0 => None,
        1 => Some("pruned 1 stale record; mur ps --verbose names each".to_string()),
        n => Some(format!(
            "pruned {n} stale records; mur ps --verbose names each"
        )),
    }
}

/// The table, or `no running capsules` when no row survived. `with_formation` adds the
/// `FORMATION` column between `UPTIME` and `URL`.
fn print_table(rows: &[(RunningRecord, &str)], with_formation: bool) {
    // A machine with no record directory and a machine with an empty one are the same fact, and
    // are reported in the same words.
    if rows.is_empty() {
        capsule_runtime::report_println!("no running capsules");
        return;
    }

    let formation_header = if with_formation {
        format!("{:<FORMATION_WIDTH$}  ", "FORMATION")
    } else {
        String::new()
    };
    capsule_runtime::report_println!(
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
        capsule_runtime::report_println!(
            "{:<SESSION_WIDTH$}  {:<CAPSULE_WIDTH$}  {:<STATUS_WIDTH$}  {:<DETACHED_WIDTH$}  {:<UPTIME_WIDTH$}  {formation}{}",
            record.session_id,
            format!("{}@{}", record.capsule_name, record.capsule_version),
            status,
            if record.outlives_launcher { "yes" } else { "no" },
            uptime(record),
            record.url.as_deref().unwrap_or("-"),
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

/// One [`ListedFormation`] per formation a row names, in the order of its first row. A formation
/// known only from `gone` records gets none, as default `mur ps` prints no line for it.
fn listed_formations(
    rows: &[(RunningRecord, &str)],
    gone: &[RunningRecord],
) -> Vec<ListedFormation> {
    let recorded = recorded_sessions(rows, gone);
    formations_in_order(rows, &[])
        .into_iter()
        .map(|formation| {
            let listed = in_formation(&formation, rows.iter().map(|(record, _)| record));
            let kept = in_formation(&formation, gone);
            let entry_member = listed
                .iter()
                .chain(&kept)
                .find(|record| record.formation_entry)
                .and_then(|record| record.formation_member.clone());
            let census = formation_census(&formation, &listed, &kept, &recorded);
            listed_formation(&formation, entry_member, listed.len(), census)
        })
        .collect()
}

/// `formation`'s entry in `formations`, from its census.
fn listed_formation(
    formation: &FormationId,
    entry_member: Option<String>,
    listed: usize,
    census: FormationCensus,
) -> ListedFormation {
    let not_listed: Vec<NotListedMember> = census
        .not_listed
        .into_iter()
        .map(|member| NotListedMember {
            session_id: member.session_id,
            formation_member: member.formation_member,
            reason: if member.ended {
                REASON_ENDED
            } else {
                REASON_GONE_WITHOUT_ENDING
            },
            record_kept: member.record_kept,
        })
        .collect();
    let ended = not_listed
        .iter()
        .filter(|member| member.reason == REASON_ENDED)
        .count();
    ListedFormation {
        formation_id: formation.as_str().to_string(),
        entry_member,
        listed,
        ended,
        gone_without_ending: not_listed.len() - ended,
        not_listed,
        searched_roots: census.searched,
        unreadable_roots: census.unreadable,
    }
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
    let statuses: Vec<&str> = rows
        .iter()
        .filter(|(record, _)| record.formation_id.as_ref() == Some(formation))
        .map(|(_, status)| *status)
        .collect();
    let census = formation_census(
        formation,
        &in_formation(formation, rows.iter().map(|(record, _)| record)),
        &in_formation(formation, pruned),
        &recorded_sessions(rows, pruned),
    );
    census_line(formation, &statuses, &census)
}

/// [`summary_line`]'s text for a formation whose listed records carry `statuses`.
fn census_line(formation: &FormationId, statuses: &[&str], census: &FormationCensus) -> String {
    let mut line = format!("formation {formation}: {} listed", statuses.len());
    if !statuses.is_empty() {
        let statuses = counted(&[
            (count_of(statuses, STATUS_RUNNING), STATUS_RUNNING),
            (count_of(statuses, STATUS_UNREACHABLE), STATUS_UNREACHABLE),
        ]);
        line.push_str(&format!(" ({statuses})"));
    }
    let pruned = census.not_listed.iter().filter(|m| m.record_kept).count();
    if pruned > 0 {
        line.push_str(&format!(", {pruned} pruned now"));
    }

    let traced_only = census.not_listed.iter().filter(|m| !m.record_kept);
    let (ended, no_record): (Vec<_>, Vec<_>) = traced_only.partition(|m| m.ended);
    if ended.len() + no_record.len() > 0 {
        let not_listed = counted(&[(ended.len(), "ended"), (no_record.len(), "with no record")]);
        line.push_str(&format!("; not listed: {not_listed}"));
    } else {
        line.push_str(&format!(
            "; no other member found in {} session {}",
            census.searched,
            if census.searched == 1 {
                "root"
            } else {
                "roots"
            }
        ));
    }
    if census.unreadable > 0 {
        line.push_str(&format!(
            "; {} session {} could not be read",
            census.unreadable,
            if census.unreadable == 1 {
                "root"
            } else {
                "roots"
            }
        ));
    }
    line
}

/// The records among `records` that name `formation`.
fn in_formation<'a>(
    formation: &FormationId,
    records: impl IntoIterator<Item = &'a RunningRecord>,
) -> Vec<&'a RunningRecord> {
    records
        .into_iter()
        .filter(|record| record.formation_id.as_ref() == Some(formation))
        .collect()
}

/// Every session this read had a record for, listed or gone, in any formation.
fn recorded_sessions<'a>(
    rows: &'a [(RunningRecord, &str)],
    gone: &'a [RunningRecord],
) -> HashSet<&'a str> {
    rows.iter()
        .map(|(record, _)| record)
        .chain(gone)
        .map(|record| record.session_id.as_str())
        .collect()
}

/// What one read knows of a formation's members beyond its listed records. Plain `mur ps`
/// renders it as [`census_line`] and `mur ps --json` as a [`ListedFormation`], so the two count
/// the same members.
#[derive(Debug)]
struct FormationCensus {
    /// Every member with no listed record, by session id descending: each gone record of the
    /// formation, then each member a trace under a searched root records that had no record in
    /// this read at all.
    not_listed: Vec<CensusMember>,
    /// Session roots that were listed.
    searched: usize,
    /// Session roots that exist and could not be listed.
    unreadable: usize,
}

/// One member in [`FormationCensus::not_listed`].
#[derive(Debug, PartialEq, Eq)]
struct CensusMember {
    session_id: String,
    /// The gone record's `formation_member`, falling back to its trace's
    /// `session_start.formation_member`; a trace-only member's comes from its trace.
    formation_member: Option<String>,
    /// Whether a trace found under a searched root holds a `session_end`.
    ended: bool,
    /// Whether this read found the member's running record with its process gone.
    record_kept: bool,
}

/// What the traces under `formation`'s session roots record about its `gone` records' sessions
/// and about members `recorded` has no record of.
///
/// The roots searched are each `listed` or `gone` record's records group and each peer's root
/// under `~/.murmur/formations/<frm_id>/`. `recorded` is every session this read had a record
/// for, in any formation, so a trace is never counted beside its own record. A member found under
/// two roots counts once. A root that cannot be listed is counted in `unreadable` and fails
/// nothing; a gone record whose trace is under it, or nowhere, did not end.
fn formation_census(
    formation: &FormationId,
    listed: &[&RunningRecord],
    gone: &[&RunningRecord],
    recorded: &HashSet<&str>,
) -> FormationCensus {
    // A record's `records_dir` is its session's records directory, so its parent is the records
    // group its siblings are in.
    let roots: BTreeSet<PathBuf> = listed
        .iter()
        .chain(gone)
        .filter_map(|record| record.records_dir.parent().map(PathBuf::from))
        // Each peer records under its own directory in the murmur home, whether or not a record
        // of it is left to point there.
        .chain(capsule_runtime::formation_launch::formation_member_roots(
            formation,
        ))
        .collect();
    let gone_sessions: HashSet<&str> = gone
        .iter()
        .map(|record| record.session_id.as_str())
        .collect();

    // Every member found, once each, with its roster name and whether it ended.
    let mut traced: HashMap<String, (Option<String>, bool)> = HashMap::new();
    let (mut searched, mut unreadable) = (0, 0);
    for root in &roots {
        let Ok(found) = recorded_members(root, formation) else {
            unreadable += 1;
            continue;
        };
        searched += 1;
        for member in found.members {
            let session = member.session_id.as_str();
            if recorded.contains(session) && !gone_sessions.contains(session) {
                continue;
            }
            traced
                .entry(member.session_id)
                .or_insert((member.member, member.exit_status.is_some()));
        }
    }

    let mut not_listed: Vec<CensusMember> = gone
        .iter()
        .map(|record| {
            let trace = traced.remove(&record.session_id);
            CensusMember {
                session_id: record.session_id.clone(),
                formation_member: record
                    .formation_member
                    .clone()
                    .or_else(|| trace.as_ref().and_then(|(member, _)| member.clone())),
                ended: trace.is_some_and(|(_, ended)| ended),
                record_kept: true,
            }
        })
        .collect();
    // What is left is every member found that had no record in this read at all.
    not_listed.extend(
        traced
            .into_iter()
            .map(|(session_id, (formation_member, ended))| CensusMember {
                session_id,
                formation_member,
                ended,
                record_kept: false,
            }),
    );
    not_listed.sort_by(|a, b| b.session_id.cmp(&a.session_id));
    FormationCensus {
        not_listed,
        searched,
        unreadable,
    }
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

    #[test]
    fn the_pruned_summary_counts_every_removal_in_one_line() {
        assert_eq!(pruned_summary(0), None);
        assert_eq!(
            pruned_summary(1).as_deref(),
            Some("pruned 1 stale record; mur ps --verbose names each")
        );
        assert_eq!(
            pruned_summary(302).as_deref(),
            Some("pruned 302 stale records; mur ps --verbose names each")
        );
    }

    fn row(session_id: &str, formation: Option<&FormationId>) -> (RunningRecord, &'static str) {
        let record = RunningRecord {
            session_id: session_id.to_string(),
            url: Some("127.0.0.1:1".to_string()),
            pid: 1,
            process_start: String::new(),
            capsule_name: "c".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/nonexistent/root").join(session_id),
            records_dir: PathBuf::from("/nonexistent/records/root").join(session_id),
            outlives_launcher: false,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            credentials: Default::default(),
            formation_id: formation.cloned(),
            formation_member: None,
            formation_entry: false,
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
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

    /// A formation's peers record under their own directories in the murmur home and its entry
    /// member under the project: their rows group as one formation, and each directory is a
    /// root searched for members not listed.
    #[test]
    fn members_recorded_under_formation_directories_group_as_one_formation() {
        let formation = FormationId::mint();
        let mut entry = row("ses_3", Some(&formation));
        entry.0.workdir = PathBuf::from("/nonexistent/project/.murmur/ses_3");
        entry.0.records_dir =
            PathBuf::from("/nonexistent/home/.murmur/records/nonexistent/project/ses_3");
        let mut worker = row("ses_2", Some(&formation));
        worker.0.workdir = PathBuf::from(format!(
            "/nonexistent/home/.murmur/formations/{formation}/worker/.murmur/ses_2"
        ));
        worker.0.records_dir = PathBuf::from(format!(
            "/nonexistent/home/.murmur/records/nonexistent/home/.murmur/formations/{formation}/worker/ses_2"
        ));
        let rows = vec![entry, row("ses_1", None), worker];
        let order: Vec<String> = grouped(rows.clone())
            .into_iter()
            .map(|(record, _)| record.session_id)
            .collect();
        assert_eq!(order, ["ses_3", "ses_2", "ses_1"]);
        assert_eq!(
            summary_line(&formation, &rows, &[]),
            format!(
                "formation {formation}: 2 listed (2 running); no other member found in 2 session \
                 roots"
            )
        );
    }

    /// A `ses_` id minted `offset_ms` after `formation`, so a census does not skip its trace.
    fn session_after(formation: &FormationId, offset_ms: u64, tail: &str) -> String {
        format!(
            "ses_{:012x}{tail:0>20}",
            formation.minted_at_ms() + offset_ms
        )
    }

    /// A record of `formation` whose records directory is `<root>/<session_id>`.
    fn member_at(
        root: &std::path::Path,
        session_id: &str,
        formation: &FormationId,
        member: Option<&str>,
    ) -> RunningRecord {
        let (mut record, _) = row(session_id, Some(formation));
        record.records_dir = root.join(session_id);
        record.formation_member = member.map(str::to_string);
        record
    }

    /// Writes `<root>/<session_id>/trace.jsonl`: a `session_start` naming `formation` and
    /// `member`, then a `session_end` when `ended`.
    fn write_trace(
        root: &std::path::Path,
        session_id: &str,
        formation: &FormationId,
        member: Option<&str>,
        ended: bool,
    ) {
        let mut start = serde_json::json!({
            "event_type": "session_start",
            "session_id": session_id,
            "capsule_name": "member",
            "capsule_version": "0.1.0",
            "model": "m",
            "max_turns": 3,
            "formation_id": formation.as_str(),
        });
        if let Some(member) = member {
            start["formation_member"] = member.into();
        }
        let mut lines = vec![start.to_string()];
        if ended {
            lines.push(
                serde_json::json!({"event_type": "session_end", "exit_status": "completed"})
                    .to_string(),
            );
        }
        let dir = root.join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("trace.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    /// A gone record is counted with its record kept: `ended` when its trace holds a
    /// `session_end`, with the trace's roster name when the record names none, and
    /// `gone_without_ending` with the record's roster name when it has no trace. A member known
    /// only from its trace has no record kept, and a `session_start` naming no member leaves its
    /// roster name `null`. A listed member's trace, which has no `session_end` yet, is not
    /// counted, and the plain line counts the same members.
    #[test]
    fn a_census_counts_gone_records_and_trace_only_members_by_their_traces() {
        let formation = FormationId::mint();
        let records = tempfile::tempdir().unwrap();
        let root = records.path().join("project");
        let listed = session_after(&formation, 50, "5");
        let gone_ended = session_after(&formation, 40, "4");
        let gone_untraced = session_after(&formation, 30, "3");
        let traced_ended = session_after(&formation, 20, "2");
        let traced_child = session_after(&formation, 10, "1");
        write_trace(&root, &listed, &formation, Some("lead"), false);
        write_trace(&root, &gone_ended, &formation, Some("coder"), true);
        write_trace(&root, &traced_ended, &formation, Some("reviewer"), true);
        write_trace(&root, &traced_child, &formation, None, false);

        let rows = vec![(
            member_at(&root, &listed, &formation, Some("lead")),
            STATUS_RUNNING,
        )];
        let gone = vec![
            member_at(&root, &gone_ended, &formation, None),
            member_at(&root, &gone_untraced, &formation, Some("worker")),
        ];
        let entries = serde_json::to_value(listed_formations(&rows, &gone)).unwrap();
        assert_eq!(
            entries,
            serde_json::json!([{
                "formation_id": formation.as_str(),
                "entry_member": null,
                "listed": 1,
                "ended": 2,
                "gone_without_ending": 2,
                "not_listed": [
                    {"session_id": gone_ended, "formation_member": "coder",
                     "reason": "ended", "record_kept": true},
                    {"session_id": gone_untraced, "formation_member": "worker",
                     "reason": "gone_without_ending", "record_kept": true},
                    {"session_id": traced_ended, "formation_member": "reviewer",
                     "reason": "ended", "record_kept": false},
                    {"session_id": traced_child, "formation_member": null,
                     "reason": "gone_without_ending", "record_kept": false},
                ],
                "searched_roots": 1,
                "unreadable_roots": 0,
            }])
        );
        assert_eq!(
            summary_line(&formation, &rows, &gone),
            format!(
                "formation {formation}: 1 listed (1 running), 2 pruned now; not listed: 1 ended, \
                 1 with no record"
            )
        );
    }

    /// Every capsule entry carries `formation_entry`, `false` for a standalone session, right
    /// after `formation_member`.
    #[test]
    fn every_listed_capsule_says_whether_it_is_a_formation_entry() {
        let (standalone, status) = row("ses_1", None);
        let capsule = listed_capsule(standalone, status);
        assert_eq!(
            serde_json::to_value(&capsule).unwrap()["formation_entry"],
            false
        );
        let text = serde_json::to_string(&capsule).unwrap();
        assert!(
            text.ends_with(r#""formation_member":null,"formation_entry":false}"#),
            "{text}"
        );

        let formation = FormationId::mint();
        let (mut entry, status) = row("ses_2", Some(&formation));
        entry.formation_member = Some("lead".to_string());
        entry.formation_entry = true;
        let value = serde_json::to_value(listed_capsule(entry, status)).unwrap();
        assert_eq!(value["formation_member"], "lead");
        assert_eq!(value["formation_entry"], true);
    }

    /// A formation's `entry_member` is the roster name of the record that says it is the entry
    /// member — a listed one before a gone one kept on disk — and `null` when no record in the
    /// read says so.
    #[test]
    fn a_formations_entry_member_comes_from_the_record_marked_entry() {
        let formation = FormationId::mint();
        let records = tempfile::tempdir().unwrap();
        let root = records.path().join("project");
        let (lead, worker, old_lead) = (
            session_after(&formation, 30, "3"),
            session_after(&formation, 20, "2"),
            session_after(&formation, 10, "1"),
        );
        let mut lead_record = member_at(&root, &lead, &formation, Some("lead"));
        lead_record.formation_entry = true;
        let worker_record = member_at(&root, &worker, &formation, Some("worker"));
        let mut gone_lead = member_at(&root, &old_lead, &formation, Some("planner"));
        gone_lead.formation_entry = true;

        let rows = vec![
            (lead_record.clone(), STATUS_RUNNING),
            (worker_record.clone(), STATUS_RUNNING),
        ];
        let entries = listed_formations(&rows, std::slice::from_ref(&gone_lead));
        assert_eq!(entries[0].entry_member.as_deref(), Some("lead"));

        let peers_only = vec![(worker_record.clone(), STATUS_RUNNING)];
        let entries = listed_formations(&peers_only, std::slice::from_ref(&gone_lead));
        assert_eq!(entries[0].entry_member.as_deref(), Some("planner"));

        let entries = listed_formations(&peers_only, &[]);
        assert_eq!(entries[0].entry_member, None);
        assert_eq!(
            serde_json::to_value(&entries[0]).unwrap()["entry_member"],
            serde_json::Value::Null
        );
    }

    /// A formation known only from gone records gets no entry, and its gone record, which this
    /// read had a record for, is never counted as a trace-only member of another formation.
    #[test]
    fn a_formation_known_only_from_gone_records_gets_no_entry() {
        let listed_formation = FormationId::mint();
        let gone_formation = FormationId::mint();
        let records = tempfile::tempdir().unwrap();
        let root = records.path().join("project");
        let listed = session_after(&listed_formation, 20, "2");
        let other = session_after(&listed_formation, 10, "1");
        write_trace(&root, &listed, &listed_formation, None, false);
        write_trace(&root, &other, &listed_formation, None, true);

        let rows = vec![
            (
                member_at(&root, &listed, &listed_formation, None),
                STATUS_RUNNING,
            ),
            row("ses_0", None),
        ];
        // A record of another formation whose session's trace names the listed one, which no
        // real trace does, so only `recorded` keeps it out.
        let gone = vec![member_at(&root, &other, &gone_formation, None)];
        let entries = listed_formations(&rows, &gone);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].formation_id, listed_formation.as_str());
        assert_eq!(entries[0].listed, 1);
        assert!(entries[0].not_listed.is_empty());
        assert_eq!(
            summary_line(&listed_formation, &rows, &gone),
            format!(
                "formation {listed_formation}: 1 listed (1 running); no other member found in 1 \
                 session root"
            )
        );
        assert_eq!(
            summary_line(&gone_formation, &rows, &gone),
            format!(
                "formation {gone_formation}: 0 listed, 1 pruned now; no other member found in 1 \
                 session root"
            )
        );
        assert!(listed_formations(&[row("ses_0", None)], &gone).is_empty());
    }

    /// A member whose trace is under two searched roots counts once.
    #[test]
    fn a_member_found_under_two_roots_counts_once() {
        let formation = FormationId::mint();
        let records = tempfile::tempdir().unwrap();
        let (first, second) = (records.path().join("a"), records.path().join("b"));
        let (entry, peer, ended) = (
            session_after(&formation, 30, "3"),
            session_after(&formation, 20, "2"),
            session_after(&formation, 10, "1"),
        );
        write_trace(&first, &ended, &formation, Some("worker"), true);
        write_trace(&second, &ended, &formation, Some("worker"), true);
        let rows = vec![
            (member_at(&first, &entry, &formation, None), STATUS_RUNNING),
            (member_at(&second, &peer, &formation, None), STATUS_RUNNING),
        ];
        let entries = listed_formations(&rows, &[]);
        assert_eq!(entries[0].not_listed.len(), 1);
        assert_eq!(entries[0].ended, 1);
        assert_eq!(entries[0].searched_roots, 2);
        assert_eq!(
            summary_line(&formation, &rows, &[]),
            format!("formation {formation}: 2 listed (2 running); not listed: 1 ended")
        );
    }

    /// A gone record whose trace is under a root that cannot be listed did not end, as far as
    /// this read knows, and the root is counted unreadable rather than failing anything.
    #[test]
    fn a_gone_record_under_an_unreadable_root_is_gone_without_ending() {
        use std::os::unix::fs::PermissionsExt;
        let formation = FormationId::mint();
        let records = tempfile::tempdir().unwrap();
        let (readable, sealed) = (records.path().join("a"), records.path().join("b"));
        let (entry, gone) = (
            session_after(&formation, 20, "2"),
            session_after(&formation, 10, "1"),
        );
        std::fs::create_dir_all(&readable).unwrap();
        write_trace(&sealed, &gone, &formation, Some("worker"), true);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&sealed).is_ok() {
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: a mode-000 directory is readable here, as it is to root");
            return;
        }
        let rows = vec![(
            member_at(&readable, &entry, &formation, None),
            STATUS_RUNNING,
        )];
        let gone = vec![member_at(&sealed, &gone, &formation, None)];
        let entries = listed_formations(&rows, &gone);
        let line = summary_line(&formation, &rows, &gone);
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(entries[0].gone_without_ending, 1);
        assert_eq!(entries[0].not_listed[0].reason, REASON_GONE_WITHOUT_ENDING);
        assert!(entries[0].not_listed[0].record_kept);
        assert_eq!(entries[0].not_listed[0].formation_member, None);
        assert_eq!(entries[0].searched_roots, 1);
        assert_eq!(entries[0].unreadable_roots, 1);
        assert_eq!(
            line,
            format!(
                "formation {formation}: 1 listed (1 running), 1 pruned now; no other member \
                 found in 1 session root; 1 session root could not be read"
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
