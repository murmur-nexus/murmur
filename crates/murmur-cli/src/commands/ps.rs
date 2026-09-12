//! The capsules running on this machine.

use capsule_runtime::{running, Liveness, RunningRecord};
use chrono::{DateTime, Utc};

use crate::error::CliError;

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

/// What a row says about the capsule it names, once the record has been verified.
const STATUS_RUNNING: &str = "running";
const STATUS_UNREACHABLE: &str = "unreachable";

/// List every capsule running on this machine, verifying each record before printing it.
///
/// Host-scoped, exactly like `docker ps`. A capsule deployed onto another machine writes its
/// record on *that* machine, so it is that machine's `mur ps` that lists it.
///
/// The listing is also the reaper: a record whose process is gone is unlinked on the way past.
/// A record whose process is alive and whose door is quiet is kept and reported as `unreachable`
/// — a slow door is not evidence of a dead capsule, and unlinking it would throw away the only
/// handle to something still running.
pub(crate) fn run_ps() -> Result<(), CliError> {
    let mut rows = Vec::new();
    for record in running::list() {
        match running::verify(&record) {
            Liveness::Live => rows.push((record, STATUS_RUNNING)),
            Liveness::Unreachable(_) => rows.push((record, STATUS_UNREACHABLE)),
            Liveness::Gone(_) => running::prune(&record),
        }
    }

    // A machine with no record directory and a machine with an empty one are the same fact, and
    // are reported in the same words.
    if rows.is_empty() {
        println!("no running capsules");
        return Ok(());
    }

    println!(
        "{:<SESSION_WIDTH$}  {:<CAPSULE_WIDTH$}  {:<STATUS_WIDTH$}  {:<DETACHED_WIDTH$}  {:<UPTIME_WIDTH$}  URL",
        "SESSION", "CAPSULE", "STATUS", "DETACHED", "UPTIME"
    );
    for (record, status) in &rows {
        println!(
            "{:<SESSION_WIDTH$}  {:<CAPSULE_WIDTH$}  {:<STATUS_WIDTH$}  {:<DETACHED_WIDTH$}  {:<UPTIME_WIDTH$}  {}",
            record.session_id,
            format!("{}@{}", record.capsule_name, record.capsule_version),
            status,
            if record.outlives_launcher { "yes" } else { "no" },
            uptime(record),
            record.url,
        );
    }

    Ok(())
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
