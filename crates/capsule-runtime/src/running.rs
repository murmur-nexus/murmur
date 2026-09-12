//! The machine-wide map of capsules that are currently reachable: one record per session that
//! opened an A2A door, at `~/.murmur/running/<session_id>.json`.
//!
//! A record is a **hint, never a truth**. Nothing can guarantee it is removed — a `SIGKILL`ed
//! capsule writes no farewell — so a reader never trusts one. [`verify`] puts three layers between
//! a record and the claim that a session is live, and each is insufficient alone:
//!
//! 1. **The pid is alive.** Necessary, nowhere near sufficient.
//! 2. **The process's start time matches the recorded one.** Pids are reused, so without this a
//!    record can name an unrelated process that happens to hold the number now.
//! 3. **The door answers and identifies itself.** A live pid proves neither that the process is
//!    the capsule nor that it can still respond, so the agent card is fetched and its `session_id`
//!    compared.
//!
//! Reading prunes what fails layer 1 or 2, and that is the only reaper there is — which is enough
//! precisely because the record is a hint. A record that passes 1 and 2 and fails 3 is kept: it
//! names a process that is genuinely alive, possibly mid-turn, and unlinking it would throw away
//! the only handle to a running capsule because it was slow to answer.
//!
//! The record names the process and stores nothing from the environment. The directory is `0700`
//! and each record `0600`, because the set of records is a map of reachable capsules to anything
//! on the machine that can read it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Directory under the murmur home holding one record per running session.
const RUNNING_DIR: &str = "running";

/// Mode held on the running directory, and on `~/.murmur` on the way to it.
const RUNNING_DIR_MODE: u32 = 0o700;

/// Mode held on each record file.
const RECORD_FILE_MODE: u32 = 0o600;

/// How long the door probe waits for a TCP connection: long enough for a loopback accept, short
/// enough that an address nothing holds is reported rather than waited on.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the door probe waits for the agent card once connected. Longer than the connect
/// deadline: a capsule mid-turn has answered, and is allowed to be slow about the rest.
const PROBE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Where one running session's door is, and which process holds it.
///
/// Every field is required. There is no version field: a record that does not deserialize is
/// unverifiable, and an unverifiable record is pruned, which is what "the record is a hint"
/// already means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningRecord {
    /// The session this door answers for, compared against the agent card by layer 3.
    pub session_id: String,
    /// `host:port` the A2A door is bound to.
    pub url: String,
    /// The process holding the door.
    pub pid: u32,
    /// That process's start time, as an opaque platform-local token — see [`process_start_token`].
    pub process_start: String,
    pub capsule_name: String,
    pub capsule_version: String,
    /// The session directory, so a reader can find the trace of what it is watching.
    pub workdir: PathBuf,
    /// Whether the capsule survives the window that launched it — `true` when the launching
    /// process had no controlling terminal.
    pub outlives_launcher: bool,
    /// RFC 3339, in UTC.
    pub started_at: String,
}

/// `~/.murmur/running`, created if missing and held at `0700`.
///
/// The directory is global rather than per-workdir because the question a live address answers is
/// "what is running on this machine", which no single project directory can answer.
pub fn running_dir() -> Result<PathBuf, String> {
    let dir = crate::state_store::murmur_home_dir()?.join(RUNNING_DIR);
    crate::state_store::ensure_private_dir(&dir, RUNNING_DIR_MODE)?;
    Ok(dir)
}

/// Every record that parses, most recent session first.
///
/// Session ids are time-ordered, so a lexical sort descending is chronological. A file that does
/// not parse is unlinked on the way past: nothing can verify it, and leaving it would mean
/// carrying a permanent unreadable entry in a directory whose whole purpose is to be read.
pub fn list() -> Vec<RunningRecord> {
    let Ok(dir) = running_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        match read_record(&path) {
            Some(record) => records.push(record),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    records.sort_by(|left, right| right.session_id.cmp(&left.session_id));
    records
}

/// What layers 1 and 2 say about the process a record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// The pid is held by the process that wrote the record.
    Alive,
    /// The process is gone; the string says how that was decided.
    Gone(String),
}

/// Layers 1 and 2: the pid is alive, and the process holding it is the one that wrote the record.
///
/// Local and cheap, which is what makes this the candidacy test — a reader can apply it to every
/// record without a single network round trip.
pub fn process_state(record: &RunningRecord) -> ProcessState {
    if !pid_is_alive(record.pid) {
        return ProcessState::Gone(format!("no process holds pid {}", record.pid));
    }
    match process_start_token(record.pid) {
        Some(token) if token == record.process_start => ProcessState::Alive,
        Some(_) => ProcessState::Gone(format!(
            "pid {} is held by a process that started at another time",
            record.pid
        )),
        None => ProcessState::Gone(format!("pid {}'s start time could not be read", record.pid)),
    }
}

/// What all three layers say about a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// The process is the one that wrote the record and its door answers for that session.
    Live,
    /// The process is alive and the door did not answer for it. The record is kept.
    Unreachable(String),
    /// The process that wrote the record is gone. The record is stale and can be pruned.
    Gone(String),
}

/// All three layers, door probe included.
///
/// The probe is one blocking `GET /.well-known/agent-card.json`, so this is callable from outside
/// any async runtime.
pub fn verify(record: &RunningRecord) -> Liveness {
    match process_state(record) {
        ProcessState::Gone(reason) => Liveness::Gone(reason),
        ProcessState::Alive => match probe_session_id(&record.url) {
            Ok(session_id) if session_id == record.session_id => Liveness::Live,
            Ok(other) => Liveness::Unreachable(format!(
                "the capsule at {} answers for session {other}",
                record.url
            )),
            Err(reason) => Liveness::Unreachable(reason),
        },
    }
}

/// What sending one signal to a record's process did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOutcome {
    /// The signal was delivered to the process that wrote the record.
    Sent,
    /// Nothing was signalled: the process that wrote the record is no longer there.
    AlreadyGone,
    /// The process is there and the kernel refused the signal; the string is the errno
    /// description, `EPERM` included.
    Refused(String),
}

/// `SIGTERM` the process a record names, if it is still that process.
pub fn signal_term(record: &RunningRecord) -> SignalOutcome {
    signal(record, libc::SIGTERM)
}

/// `SIGKILL` the process a record names, if it is still that process.
pub fn signal_kill(record: &RunningRecord) -> SignalOutcome {
    signal(record, libc::SIGKILL)
}

/// Re-verify layers 1 and 2, then signal — in that order, in this one place.
///
/// A record is a hint, and a pid whose recorded start time no longer matches names a process that
/// inherited the number. Signalling it would end something the operator never launched, so the
/// check is not a caller's to have made earlier: it is re-run here, immediately before the
/// `kill(2)`, so the window between the two is two adjacent syscalls. No userspace program can
/// close that window; narrowing it to this is the whole of what can be done about it.
#[allow(unsafe_code)]
fn signal(record: &RunningRecord, signal: libc::c_int) -> SignalOutcome {
    if matches!(process_state(record), ProcessState::Gone(_)) {
        return SignalOutcome::AlreadyGone;
    }
    // SAFETY: `kill` takes a pid and a signal number by value and dereferences no pointer. The
    // pid is the one the check above just confirmed is held by the process that wrote the record.
    if unsafe { libc::kill(record.pid as libc::pid_t, signal) } == 0 {
        return SignalOutcome::Sent;
    }
    let err = std::io::Error::last_os_error();
    // The process exited between the check and the call — the race this ordering narrows but
    // cannot close. Nothing was signalled, and nothing needed to be.
    if err.raw_os_error() == Some(libc::ESRCH) {
        return SignalOutcome::AlreadyGone;
    }
    SignalOutcome::Refused(err.to_string())
}

/// Unlink one record. For a [`Liveness::Gone`] reading only.
pub fn prune(record: &RunningRecord) {
    if let Ok(dir) = running_dir() {
        let _ = std::fs::remove_file(record_path(&dir, &record.session_id));
    }
}

/// One session's record, written for exactly as long as the session runs.
///
/// `launch_session` has one `?` per staging step and two success returns, so removal is a guard
/// rather than a line at each of them: a session that ended without retiring its record would
/// leave an address that resolves to a port nothing holds.
pub struct RunningGuard {
    path: PathBuf,
}

impl RunningGuard {
    /// Writes `record` and holds it until this guard drops.
    ///
    /// The `Err` names why the record could not be written. Failing to record never fails a
    /// launch: the caller warns and runs on, and what it has lost is the session being addressable
    /// by session address.
    pub fn write(record: &RunningRecord) -> Result<Self, String> {
        let dir = running_dir()?;
        let path = record_path(&dir, &record.session_id);
        let body = serde_json::to_vec_pretty(record)
            .map_err(|err| format!("the record could not be serialized: {err}"))?;

        // Staged beside the record and renamed onto it, because a reader prunes what it cannot
        // parse: a reader landing between the create and the write would otherwise unlink a
        // record that was about to be complete.
        crate::retention::StagedRewrite::stage_with_mode(&path, &body, Some(RECORD_FILE_MODE))
            .and_then(crate::retention::StagedRewrite::commit)
            .map_err(|reason| format!("failed to write {}: {reason}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether the calling process has a controlling terminal, which is what decides
/// `outlives_launcher`: a capsule attached to a terminal dies with that window.
///
/// `/dev/tty` rather than `isatty` on stdio, so redirecting a stream does not change the answer.
#[must_use]
pub fn has_controlling_terminal() -> bool {
    std::fs::File::open("/dev/tty").is_ok()
}

fn record_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

fn read_record(path: &Path) -> Option<RunningRecord> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Layer 1. `EPERM` counts as alive: the pid is held, by a process this user may not signal.
///
/// A zombie does not. It still holds its pid and still answers `kill(pid, 0)`, but it has exited
/// — its door is closed and its memory reclaimed, and it stays in the table only until whoever
/// started it waits on it. Counting one as alive would mean `mur ps` listing an exited capsule as
/// `unreachable` for as long as its launcher neglected to reap it, and `mur stop` never able to
/// confirm that the process it just signalled had gone.
#[allow(unsafe_code)]
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 runs the existence and permission checks without delivering
    // anything, and dereferences no pointer.
    let held = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    held && !platform::is_zombie(pid)
}

/// Layer 2's reading: an opaque, platform-local token for when `pid`'s process started.
///
/// Compared only by string equality against a token read freshly on the same machine. Nothing
/// converts between platform encodings and nothing parses either one, so the two readings below
/// never have to be reconciled.
///
/// | Platform | Token |
/// |---|---|
/// | Linux | field 22 of `/proc/<pid>/stat`, clock ticks since boot |
/// | macOS | `kinfo_proc.kp_proc.p_starttime`, as `seconds.microseconds` |
#[must_use]
pub fn process_start_token(pid: u32) -> Option<String> {
    platform::process_start_token(pid)
}

#[cfg(target_os = "linux")]
mod platform {
    /// Field 22 of `/proc/<pid>/stat`.
    ///
    /// The second field is the executable name in parentheses and may itself contain spaces and
    /// parentheses, so the split starts after its last `)`: the remainder begins at field 3.
    /// Field 22, counted from the field after the executable name, which is field 3.
    const STARTTIME_OFFSET_AFTER_COMM: usize = 22 - 3;

    pub(super) fn process_start_token(pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let tail = &stat[stat.rfind(')')? + 1..];
        tail.split_whitespace()
            .nth(STARTTIME_OFFSET_AFTER_COMM)
            .map(str::to_string)
    }

    /// Field 3 of `/proc/<pid>/stat` — the first after the executable name — is the state
    /// character, and `Z` is a process that has exited and not yet been waited on.
    ///
    /// A pid whose `stat` cannot be read is not reported as a zombie: the reading failed, which
    /// says nothing about the process, and layer 2 refuses it a moment later anyway.
    pub(super) fn is_zombie(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let Some(close) = stat.rfind(')') else {
            return false;
        };
        stat[close + 1..].split_whitespace().next() == Some("Z")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    /// `kinfo_proc.kp_proc.p_starttime`, rendered as `seconds.microseconds`.
    #[allow(unsafe_code)]
    pub(super) fn process_start_token(pid: u32) -> Option<String> {
        let mut mib: [libc::c_int; 4] = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        // SAFETY: `kinfo_proc` is a plain C struct of integers, pointers and nested structs with
        // no niche and no validity invariant, so an all-zero value is a valid one. It is
        // overwritten by the `sysctl` below before anything reads it.
        let mut info: libc::kinfo_proc = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::kinfo_proc>();

        // SAFETY: `mib` is a four-element MIB as `KERN_PROC_PID` requires, and the output buffer
        // is one `kinfo_proc` with `size` set to its own size, which is what the call writes at
        // most. A pid the kernel does not know returns a non-zero status or a zero length, both
        // of which are handled below rather than read.
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                std::ptr::addr_of_mut!(info).cast(),
                std::ptr::addr_of_mut!(size),
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 || size == 0 {
            return None;
        }
        let started = info.kp_proc.p_starttime;
        Some(format!("{}.{:06}", started.tv_sec, started.tv_usec))
    }

    /// `kinfo_proc.kp_proc.p_stat` is `SZOMB` for a process that has exited and not yet been
    /// waited on. A pid the kernel will not describe is not reported as a zombie, on the same
    /// terms as the Linux reading.
    #[allow(unsafe_code)]
    pub(super) fn is_zombie(pid: u32) -> bool {
        let mut mib: [libc::c_int; 4] = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            pid as libc::c_int,
        ];
        // SAFETY: as in `process_start_token` above — `kinfo_proc` is a plain C struct with no
        // validity invariant, so an all-zero value is valid, and it is overwritten by the
        // `sysctl` before anything reads it.
        let mut info: libc::kinfo_proc = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::kinfo_proc>();
        // SAFETY: a four-element `KERN_PROC_PID` MIB and an output buffer of exactly one
        // `kinfo_proc`, with `size` set to its own size.
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                std::ptr::addr_of_mut!(info).cast(),
                std::ptr::addr_of_mut!(size),
                std::ptr::null_mut(),
                0,
            )
        };
        status == 0 && size != 0 && i32::from(info.kp_proc.p_stat) == libc::SZOMB
    }
}

/// Layer 3: the session id the door at `url` claims on its agent card.
fn probe_session_id(url: &str) -> Result<String, String> {
    let addr = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    crate::http_client::http_json_with_timeouts(
        "GET",
        &format!("http://{addr}/.well-known/agent-card.json"),
        None,
        &[("Accept", "application/json")],
        PROBE_CONNECT_TIMEOUT,
        PROBE_READ_TIMEOUT,
    )?
    .get("session_id")
    .and_then(serde_json::Value::as_str)
    .map(str::to_string)
    .ok_or_else(|| format!("the agent card from {addr} names no session"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pid: u32, process_start: &str) -> RunningRecord {
        RunningRecord {
            session_id: "ses_0199c4e2f1b7712a9d3e4f5061728394".to_string(),
            url: "127.0.0.1:1".to_string(),
            pid,
            process_start: process_start.to_string(),
            capsule_name: "demo".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/tmp/demo"),
            outlives_launcher: true,
            started_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    /// The serialized shape is the whole contract with the CLI and with an operator reading the
    /// file, so the key set is asserted rather than assumed.
    #[test]
    fn a_record_serializes_exactly_the_nine_fields() {
        let value = serde_json::to_value(record(1, "42")).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capsule_name",
                "capsule_version",
                "outlives_launcher",
                "pid",
                "process_start",
                "session_id",
                "started_at",
                "url",
                "workdir",
            ]
        );
    }

    /// Every field is required, so a record missing one is unverifiable rather than partly
    /// usable — which is what makes pruning it on read the whole of the compatibility story.
    #[test]
    fn a_record_missing_a_field_does_not_parse() {
        let mut value = serde_json::to_value(record(1, "42")).unwrap();
        value.as_object_mut().unwrap().remove("process_start");
        assert!(serde_json::from_value::<RunningRecord>(value).is_err());
    }

    /// This process is alive and its own start time is what the host reports for it.
    #[test]
    fn this_process_reads_as_alive() {
        let pid = std::process::id();
        let token = process_start_token(pid).expect("this process has a start time");
        assert_eq!(process_state(&record(pid, &token)), ProcessState::Alive);
    }

    /// Layer 2 alone: the pid is this live process, and the recorded start time is not its.
    #[test]
    fn a_start_time_that_disagrees_reads_as_gone() {
        let pid = std::process::id();
        let state = process_state(&record(pid, "not-this-process"));
        match state {
            ProcessState::Gone(reason) => assert!(
                reason.contains("another time"),
                "the reason must say the pid belongs to another process now: {reason}"
            ),
            ProcessState::Alive => panic!("a mismatched start time must not read as alive"),
        }
    }

    /// A pid no process holds fails layer 1, whatever start time the record carries.
    #[test]
    fn a_pid_nothing_holds_reads_as_gone() {
        // The kernel hands out pids below this only after wrapping, and pid 0 is never a process.
        let state = process_state(&record(0, "42"));
        assert!(matches!(state, ProcessState::Gone(_)));
    }

    /// The token is read the same way twice, which is the only property string comparison needs.
    #[test]
    fn the_start_token_is_stable_for_one_process() {
        let pid = std::process::id();
        assert_eq!(process_start_token(pid), process_start_token(pid));
        assert!(process_start_token(pid).is_some_and(|token| !token.is_empty()));
    }

    /// A process that has exited and not been waited on still holds its pid and still answers
    /// `kill(pid, 0)`, and is not running.
    #[test]
    fn a_zombie_does_not_read_as_alive() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("sh should start");
        let pid = child.id();
        let token = process_start_token(pid).expect("a live child has a start time");

        // Deliberately not waited on: the pid stays in the table as a zombie until it is.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(process_state(&record(pid, &token)), ProcessState::Gone(_)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a zombie kept reading as alive"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.wait();
    }

    /// The load-bearing invariant: a record whose recorded start time no longer matches names a
    /// process that inherited the pid, and nothing is signalled at it.
    #[test]
    fn a_signal_at_a_mismatched_start_time_is_not_sent() {
        // This process is alive and would die of a `SIGKILL` it were actually sent one.
        let record = record(std::process::id(), "not-this-process");
        assert_eq!(signal_kill(&record), SignalOutcome::AlreadyGone);
        assert_eq!(signal_term(&record), SignalOutcome::AlreadyGone);
    }

    /// A pid nothing holds is reported as already gone rather than as a refusal — there is
    /// nothing there to refuse.
    #[test]
    fn a_signal_at_a_pid_nothing_holds_is_already_gone() {
        assert_eq!(signal_term(&record(0, "42")), SignalOutcome::AlreadyGone);
    }

    /// A live process whose door answers nothing is unreachable, not gone — the distinction the
    /// whole three-layer split exists for.
    #[test]
    fn a_live_process_behind_a_silent_port_is_unreachable() {
        let pid = std::process::id();
        let token = process_start_token(pid).unwrap();
        let mut record = record(pid, &token);
        // Bound and immediately dropped: the port is one nothing listens on.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        record.url = format!("127.0.0.1:{port}");
        assert!(matches!(verify(&record), Liveness::Unreachable(_)));
    }
}
