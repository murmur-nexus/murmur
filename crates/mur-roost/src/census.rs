//! The capsules already running on the host when the daemon starts, read from
//! `~/.murmur/running` so a restart does not reset the machine ceiling to zero.
//!
//! The count errs toward refusing. A record stops counting only on the evidence `mur ps` prunes on
//! — no process holds its pid, or the process holding it started at another time — which is
//! [`ProcessState::Gone`]. [`ProcessState::Unverified`] counts, because something holds the pid
//! and it may be the capsule. A daemon cannot tell which survivors its predecessor admitted, so
//! every live record counts, roots started by hand among them; that over-count shrinks as they
//! exit. What it cannot see goes uncounted: a session whose record could not be written, a script
//! capsule, which writes none, a record that does not parse, and capsules under another `HOME`.
//!
//! There is no door probe. Layer 3, the agent-card probe `mur ps` runs, can only turn a live
//! process into an unreachable one, and both still hold memory, which is what the ceiling
//! protects; probing would cost up to the probe's connect and read timeouts per record and decide
//! nothing.
//!
//! Nothing is written. The directory belongs to the runtime: it is listed once with
//! [`running::scan`], never created, re-moded, pruned or rewritten, and liveness is re-read in
//! memory after that. A held record carries no door token.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use capsule_runtime::running::{self, ProcessState, RunningRecord};

use crate::JobRecord;

/// The records that read as live when the daemon started, minus every one since found gone.
///
/// Only shrinks: nothing is added after [`inherit`], and a record leaves the first time a census
/// reads its process as [`ProcessState::Gone`]. `Default` is an empty host.
#[derive(Debug, Default)]
pub struct Inherited {
    records: Vec<RunningRecord>,
}

impl Inherited {
    /// How many held records still name a live capsule that the job store does not already count.
    ///
    /// Re-reads layers 1 and 2 for every held record and forgets each one now gone, which is how a
    /// slot comes back. A held session that is a key in `jobs` contributes nothing, because
    /// [`crate::bounds::live_capsules`] counts it there.
    pub fn live(&mut self, jobs: &HashMap<String, JobRecord>) -> u32 {
        self.live_with(jobs, running::process_state)
    }

    /// [`Inherited::live`] with the process reading supplied by the caller.
    pub(crate) fn live_with(
        &mut self,
        jobs: &HashMap<String, JobRecord>,
        state_of: impl Fn(&RunningRecord) -> ProcessState,
    ) -> u32 {
        self.records
            .retain(|record| !matches!(state_of(record), ProcessState::Gone(_)));
        self.records
            .iter()
            .filter(|record| !jobs.contains_key(&record.session_id))
            .count() as u32
    }

    /// How many records are held, whether or not the job store also counts them.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// What the startup census found, for the line the daemon prints before it listens.
///
/// `counted + stale + unreadable == records_read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritReport {
    /// The running directory that was read.
    pub dir: PathBuf,
    /// `.json` files read, whether or not they parsed.
    pub records_read: usize,
    /// Records whose process read as alive or unverified, and which the ceiling counts.
    pub counted: u32,
    /// Records whose process is gone. Left on disk for `mur ps` to prune.
    pub stale: u32,
    /// `.json` files that did not parse. Left on disk.
    pub unreadable: u32,
    /// How long the read and the classification took.
    pub elapsed: Duration,
}

impl InheritReport {
    /// The census line `mur-roost` prints to stderr ahead of `listening on`.
    pub fn startup_line(&self) -> String {
        format!(
            "mur-roost: {} live capsules already running on this host, counted from {} \
             ({} records read in {:.1} ms; {} stale and {} unreadable not counted)",
            self.counted,
            self.dir.display(),
            self.records_read,
            self.elapsed.as_secs_f64() * 1000.0,
            self.stale,
            self.unreadable,
        )
    }
}

/// Reads `dir` once and keeps every record whose process is not gone.
///
/// A missing `dir` is an empty host. The `Err` is `dir` existing and not being listable, and names
/// the path and the OS error; the daemon refuses to start on it rather than run from a count of
/// zero.
pub fn inherit(dir: &Path) -> Result<(Inherited, InheritReport), String> {
    inherit_with(dir, running::process_state)
}

/// [`inherit`] with the process reading supplied by the caller.
pub fn inherit_with(
    dir: &Path,
    state_of: impl Fn(&RunningRecord) -> ProcessState,
) -> Result<(Inherited, InheritReport), String> {
    let started = Instant::now();
    let scan = running::scan(dir)?;
    let records_read = scan.records.len() + scan.unreadable.len();

    let mut kept = Vec::with_capacity(scan.records.len());
    let mut stale = 0u32;
    for mut record in scan.records {
        match state_of(&record) {
            ProcessState::Gone(_) => stale += 1,
            ProcessState::Alive | ProcessState::Unverified(_) => {
                record.door_token = None;
                kept.push(record);
            }
        }
    }

    let report = InheritReport {
        dir: dir.to_path_buf(),
        records_read,
        counted: kept.len() as u32,
        stale,
        unreadable: scan.unreadable.len() as u32,
        elapsed: started.elapsed(),
    };
    Ok((Inherited { records: kept }, report))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use capsule_runtime::SpawnEnvelope;

    use super::*;
    use crate::JobStatus;

    /// Pids the fake reading treats as alive and as unverified; every other pid is gone.
    const ALIVE: u32 = 101;
    const UNVERIFIED: u32 = 102;
    const GONE: u32 = 103;

    fn by_pid(record: &RunningRecord) -> ProcessState {
        match record.pid {
            ALIVE => ProcessState::Alive,
            UNVERIFIED => ProcessState::Unverified("start time unreadable".to_string()),
            _ => ProcessState::Gone("no process holds the pid".to_string()),
        }
    }

    fn write_record(dir: &Path, session_id: &str, pid: u32) {
        let record = serde_json::json!({
            "session_id": session_id,
            "url": "127.0.0.1:1",
            "pid": pid,
            "process_start": "42",
            "capsule_name": "demo",
            "capsule_version": "0.1.0",
            "workdir": "/tmp/demo",
            "outlives_launcher": true,
            "started_at": "2026-01-01T00:00:00Z",
            "door_token": "operator-door-token",
        });
        std::fs::write(
            dir.join(format!("{session_id}.json")),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
    }

    fn running_job() -> JobRecord {
        JobRecord {
            status: JobStatus::Running,
            envelope: SpawnEnvelope::default(),
            depth_remaining: 0,
            parent_session: None,
            pending: Vec::new(),
        }
    }

    #[test]
    fn alive_and_unverified_count_gone_is_stale_and_unparseable_is_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "ses_a", ALIVE);
        write_record(dir.path(), "ses_b", UNVERIFIED);
        write_record(dir.path(), "ses_c", GONE);
        let broken = dir.path().join("ses_d.json");
        std::fs::write(&broken, "{").unwrap();

        let (mut inherited, report) = inherit_with(dir.path(), by_pid).unwrap();

        assert_eq!(report.records_read, 4);
        assert_eq!(report.counted, 2);
        assert_eq!(report.stale, 1);
        assert_eq!(report.unreadable, 1);
        assert_eq!(inherited.len(), 2);
        assert_eq!(inherited.live_with(&HashMap::new(), by_pid), 2);
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{");
        assert!(dir.path().join("ses_c.json").exists());
    }

    #[test]
    fn a_held_record_counts_until_its_process_reads_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "ses_a", ALIVE);
        write_record(dir.path(), "ses_b", UNVERIFIED);
        let (mut inherited, _) = inherit_with(dir.path(), by_pid).unwrap();
        let gone: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
        let reading = |record: &RunningRecord| {
            if gone.borrow().contains(&record.session_id) {
                ProcessState::Gone("no process holds the pid".to_string())
            } else {
                by_pid(record)
            }
        };

        assert_eq!(inherited.live_with(&HashMap::new(), reading), 2);
        gone.borrow_mut().insert("ses_a".to_string());
        assert_eq!(inherited.live_with(&HashMap::new(), reading), 1);
        assert_eq!(inherited.len(), 1);

        // Forgotten for good: a pid reused later does not bring the record back.
        gone.borrow_mut().clear();
        assert_eq!(inherited.live_with(&HashMap::new(), reading), 1);
    }

    #[test]
    fn a_held_session_the_job_store_tracks_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "ses_a", ALIVE);
        write_record(dir.path(), "ses_b", ALIVE);
        let (mut inherited, _) = inherit_with(dir.path(), by_pid).unwrap();
        let jobs = HashMap::from([("ses_a".to_string(), running_job())]);

        assert_eq!(inherited.live_with(&jobs, by_pid), 1);
        assert_eq!(inherited.len(), 2);
    }

    #[test]
    fn no_held_record_carries_a_door_token() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), "ses_a", ALIVE);
        write_record(dir.path(), "ses_b", UNVERIFIED);
        let (inherited, _) = inherit_with(dir.path(), by_pid).unwrap();

        assert_eq!(inherited.records.len(), 2);
        assert!(inherited
            .records
            .iter()
            .all(|record| record.door_token.is_none()));
    }

    #[test]
    fn the_startup_line_reads_exactly() {
        let report = InheritReport {
            dir: PathBuf::from("/home/you/.murmur/running"),
            records_read: 4,
            counted: 2,
            stale: 1,
            unreadable: 1,
            elapsed: Duration::from_micros(1_300),
        };
        assert_eq!(
            report.startup_line(),
            "mur-roost: 2 live capsules already running on this host, counted from \
             /home/you/.murmur/running (4 records read in 1.3 ms; 1 stale and 1 unreadable not \
             counted)"
        );
    }

    #[test]
    fn a_missing_directory_is_an_empty_host() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("running");

        let (inherited, report) = inherit_with(&dir, by_pid).unwrap();

        assert!(inherited.is_empty());
        assert_eq!(
            (
                report.records_read,
                report.counted,
                report.stale,
                report.unreadable
            ),
            (0, 0, 0, 0)
        );
        assert!(!dir.exists());
    }
}
