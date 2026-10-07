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
//!    the capsule nor that it can still respond, so the agent card is fetched and the `sessionId`
//!    of its capsule extension compared.
//!
//! Reading prunes a record only on evidence: layer 1 finding no process, or layer 2 reading a start
//! time that differs from the recorded one, which means the pid was reused. That is the only reaper
//! there is, and it is enough because the record is a hint. A reading that could not be taken is
//! not evidence. A live pid whose start time cannot be read is [`ProcessState::Unverified`]: kept,
//! reported as unreachable, and never signalled. A record that passes 1 and 2 and fails 3 is kept
//! too: it names a process that is alive, perhaps suspended, and unlinking it would throw away the
//! only handle to a running capsule because it was slow to answer.
//!
//! The record names the process and stores nothing from the environment. A session declaring
//! `network.authentication` also records every door token it minted: the operator token, which is
//! how the host tools call its door, and one per declared credential, which `mur token` hands out.
//! Layer 3 reads the session id off the extended card with the operator token. The record is the
//! only place a token is written: `mur run` prints none. The directory is `0700` and each record
//! `0600`, because the set of records is a map of reachable capsules, and of the tokens that drive
//! them, to anything on the machine that can read it.
//!
//! A session whose manifest declares `control:` also holds `<session_id>.control` beside its
//! record: the control token, at the same mode, written before the record and removed with it.
//! It is the only place the token exists outside the process, and it is useless once the process
//! is gone, because the key that verifies it was never written anywhere.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// Directory under the murmur home holding one record per running session.
const RUNNING_DIR: &str = "running";

/// Mode held on the running directory, and on `~/.murmur` on the way to it.
const RUNNING_DIR_MODE: u32 = 0o700;

/// Mode held on each record file, and on each control token file.
const RECORD_FILE_MODE: u32 = 0o600;

/// Extension of a session's control token file, beside its `.json` record.
const CONTROL_TOKEN_EXTENSION: &str = "control";

/// How long the door probe waits for a TCP connection: long enough for a loopback accept, short
/// enough that an address nothing holds is reported rather than waited on.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the door probe waits for the agent card once connected. Longer than the connect
/// deadline: a capsule mid-turn has answered, and is allowed to be slow about the rest.
const PROBE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Where one running session's door is, and which process holds it.
///
/// Every field but `door_token`, `credentials`, `formation_id`, `formation_lifeline`,
/// `formation_launcher` and `spawned_by` is required, and each of those six is omitted when it does
/// not apply, so a standalone session's record carries the nine required keys alone. There is no version field: a
/// record that does not deserialize names no process that could be checked, and is pruned, which
/// is what "the record is a hint" already means.
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
    /// The operator token of a door declaring `network.authentication`, which the host tools
    /// present to it. Absent for a public door, and in a record an older runtime wrote. Its
    /// `Debug` is redacted, and it is serialized only here, into a file held at `0600`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "door_token_field"
    )]
    pub door_token: Option<crate::door_auth::DoorToken>,
    /// The tokens of the credentials `network.authentication.credentials` declares, by name. The
    /// operator token is `door_token`, never an entry here. Empty, and omitted, for a public door,
    /// for one declaring no credentials, and in a record an older runtime wrote. Every scope of
    /// every entry is also a scope of `door_token`, so holding them beside it exposes nothing the
    /// file did not already.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "door_credentials_field"
    )]
    pub credentials: BTreeMap<String, crate::door_auth::DoorToken>,
    /// The formation this session is a member of, so a reader can tell a formation's records from
    /// unrelated sessions on the machine. Absent for a session in no formation, and in a record an
    /// older runtime wrote. It groups records and grants nothing. A value that is not a formation
    /// id makes the whole record unreadable, which [`list`] prunes like any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formation_id: Option<crate::formation::FormationId>,
    /// The session holds a formation lifeline: its launcher ends it by closing that lifeline, and
    /// nothing else needs to. Absent when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub formation_lifeline: bool,
    /// The formation launcher that started this session, as the launcher named itself on the
    /// member's formation channel. Absent for a session no launcher started, and for a member
    /// whose launcher could not read its own start time. A value that is not an object with a
    /// `pid` and a `process_start` makes the whole record unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formation_launcher: Option<ProcessIdentity>,
    /// The session id of the session that delegated to this one. A delegated child holds its
    /// spawner's lifeline and ends with it. Absent for a session nothing delegated to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned_by: Option<String>,
}

/// A process named so that a recycled pid is never mistaken for it: its pid, and its start time as
/// [`process_start_token`] reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub process_start: String,
}

impl ProcessIdentity {
    /// The calling process. `None` when its own start time cannot be read, because an identity
    /// with no start time names no process anyone could verify.
    pub fn of_this_process() -> Option<Self> {
        let pid = std::process::id();
        Some(Self {
            pid,
            process_start: process_start_token(pid)?,
        })
    }
}

/// How the running record serializes its operator token. With [`door_credentials_field`], the one
/// place a [`crate::door_auth::DoorToken`] is serialized.
mod door_token_field {
    use crate::door_auth::DoorToken;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        token: &Option<DoorToken>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match token {
            Some(token) => serializer.serialize_some(token.expose()),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<DoorToken>, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.map(DoorToken::new))
    }
}

/// How the running record serializes its declared credentials' tokens: a JSON object of credential
/// name to token. With [`door_token_field`], the one place a [`crate::door_auth::DoorToken`] is
/// serialized.
mod door_credentials_field {
    use std::collections::BTreeMap;

    use crate::door_auth::DoorToken;
    use serde::{ser::SerializeMap, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        credentials: &BTreeMap<String, DoorToken>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(credentials.len()))?;
        for (name, token) in credentials {
            map.serialize_entry(name, token.expose())?;
        }
        map.end()
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, DoorToken>, D::Error> {
        Ok(BTreeMap::<String, String>::deserialize(deserializer)?
            .into_iter()
            .map(|(name, token)| (name, DoorToken::new(token)))
            .collect())
    }
}

/// `~/.murmur/running`, created if missing and held at `0700`, inside a `~/.murmur` held at `0700`.
///
/// The directory is global rather than per-workdir because the question a live address answers is
/// "what is running on this machine", which no single project directory can answer.
///
/// The `Err` names the path that failed and why.
pub fn running_dir() -> Result<PathBuf, String> {
    let dir = crate::murmur_home::ensure_murmur_home()?.join(RUNNING_DIR);
    crate::state_store::ensure_private_dir(&dir, RUNNING_DIR_MODE)
        .map_err(|reason| format!("{}: {reason}", dir.display()))?;
    Ok(dir)
}

/// `~/.murmur/running`, resolved from `HOME` without creating or re-moding anything.
///
/// For a reader that must leave the directory exactly as it found it — a missing directory stays
/// missing. [`running_dir`] is the resolver for anything that writes.
///
/// The `Err` is `HOME` being unset or not absolute.
pub fn running_dir_location() -> Result<PathBuf, String> {
    Ok(crate::state_store::murmur_home_dir()?.join(RUNNING_DIR))
}

/// What one pass over a running directory found.
#[derive(Debug, Default)]
pub struct RecordScan {
    /// Every record that parses, most recent session first.
    pub records: Vec<RunningRecord>,
    /// Every `.json` file that could not be read or did not parse, left where it is.
    pub unreadable: Vec<PathBuf>,
}

/// Every `.json` entry of `dir`, read and parsed, with nothing created, re-moded or removed.
///
/// Session ids are time-ordered, so a lexical sort descending is chronological. Only the `json`
/// extension is read: the staging file [`RunningGuard::write`] renames into place ends in `.tmp`,
/// so a record mid-write is neither a record nor unreadable.
///
/// A `dir` that does not exist is an empty machine. The `Err` is any other failure to list it —
/// a path that is a regular file, or one this user may not read — and names the path and the OS
/// error.
pub fn scan(dir: &Path) -> Result<RecordScan, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(RecordScan::default()),
        Err(err) => return Err(format!("{}: {err}", dir.display())),
    };

    let mut scan = RecordScan::default();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        match read_record(&path) {
            Some(record) => scan.records.push(record),
            None => scan.unreadable.push(path),
        }
    }
    scan.records
        .sort_by(|left, right| right.session_id.cmp(&left.session_id));
    Ok(scan)
}

/// Every record that parses, most recent session first.
///
/// A file that does not parse is unlinked on the way past: nothing can verify it, and leaving it
/// would mean carrying a permanent unreadable entry in a directory whose whole purpose is to be
/// read.
///
/// The `Err` is the directory being unusable — `~/.murmur/running` cannot be created, held at
/// `0700`, or listed — and names the path and the OS error. An empty machine is `Ok` with no
/// records, so a caller can tell the two apart.
pub fn list() -> Result<Vec<RunningRecord>, String> {
    let dir = running_dir()?;
    let scan = scan(&dir)?;
    for path in &scan.unreadable {
        let _ = std::fs::remove_file(path);
    }
    Ok(scan.records)
}

/// What layers 1 and 2 say about the process a record names.
///
/// Only `Gone` licenses unlinking the record. `Unverified` is a reading that failed, which says
/// nothing about the process either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// The pid is held by the process that wrote the record.
    Alive,
    /// The pid is held and its start time could not be read, so whether the holder wrote the record
    /// is unknown. The record is kept and the pid is never signalled; the string is the reason.
    Unverified(String),
    /// The process is gone — nothing holds the pid, or the process holding it started at another
    /// time than the recorded one. The string says which.
    Gone(String),
}

/// Layers 1 and 2: the pid is alive, and the process holding it is the one that wrote the record.
///
/// Local and cheap, which is what makes this the candidacy test — a reader can apply it to every
/// record without a single network round trip. The start time is read only for a live pid; see
/// [`classify`] for how the two readings combine.
pub fn process_state(record: &RunningRecord) -> ProcessState {
    process_state_of(record.pid, &record.process_start)
}

/// Layers 1 and 2 for any process named by pid and start time, record or not.
pub fn identity_state(identity: &ProcessIdentity) -> ProcessState {
    process_state_of(identity.pid, &identity.process_start)
}

impl RunningRecord {
    /// The process this record names.
    pub fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            process_start: self.process_start.clone(),
        }
    }
}

/// Layers 1 and 2 for any process recorded as `pid` with the start token `recorded_start` — a
/// running record's process, or a formation's launcher as its ownership marker names it.
pub(crate) fn process_state_of(pid: u32, recorded_start: &str) -> ProcessState {
    let alive = pid_is_alive(pid);
    let token = alive.then(|| process_start_token(pid)).flatten();
    classify(pid, recorded_start, alive, token.as_deref())
}

/// Layers 1 and 2 over readings already taken: whether `pid` is held, and its start token read
/// afterwards, `None` when it could not be read.
///
/// A recorded start of `""` is what a writer that could not read its own start time stores, and
/// it differs from every token the host reports, so that process reads as `Gone`.
fn classify(
    pid: u32,
    recorded_start: &str,
    pid_alive: bool,
    fresh_token: Option<&str>,
) -> ProcessState {
    if !pid_alive {
        return ProcessState::Gone(format!("no process holds pid {pid}"));
    }
    match fresh_token {
        Some(token) if token == recorded_start => ProcessState::Alive,
        Some(_) => ProcessState::Gone(format!(
            "pid {pid} is held by a process that started at another time"
        )),
        None => ProcessState::Unverified(format!("pid {pid}'s start time could not be read")),
    }
}

/// What all three layers say about a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// The process is the one that wrote the record and its door answers for that session.
    Live,
    /// The process is alive and either the door did not answer for it or its identity could not be
    /// read. The record is kept.
    Unreachable(String),
    /// The process that wrote the record is gone. The record is stale and can be pruned.
    Gone(String),
}

/// All three layers, door probe included.
///
/// The probe is one blocking `GET /.well-known/agent-card.json`, so this is callable from outside
/// any async runtime. A record whose process is not [`ProcessState::Alive`] is answered without a
/// probe.
pub fn verify(record: &RunningRecord) -> Liveness {
    if let Some(liveness) = liveness_without_probe(process_state(record)) {
        return liveness;
    }
    match probe_session_id(record) {
        Ok(session_id) if session_id == record.session_id => Liveness::Live,
        Ok(other) => Liveness::Unreachable(format!(
            "the capsule at {} answers for session {other}",
            record.url
        )),
        Err(reason) => Liveness::Unreachable(reason),
    }
}

/// The [`Liveness`] layers 1 and 2 decide on their own, or `None` for [`ProcessState::Alive`],
/// which only the door probe can settle.
fn liveness_without_probe(state: ProcessState) -> Option<Liveness> {
    match state {
        ProcessState::Alive => None,
        ProcessState::Unverified(reason) => Some(Liveness::Unreachable(reason)),
        ProcessState::Gone(reason) => Some(Liveness::Gone(reason)),
    }
}

/// What sending one signal to a record's process did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOutcome {
    /// The signal was delivered to the process that wrote the record.
    Sent,
    /// Nothing was signalled: the process that wrote the record is no longer there.
    AlreadyGone,
    /// Something holds the pid and no signal reached it: the kernel refused `kill(2)`, and the
    /// string is the errno description, `EPERM` included; or the pid is
    /// [`ProcessState::Unverified`], `kill(2)` was never called, and the string is that reason.
    Refused(String),
}

/// `SIGTERM` the process a record names, if it is still that process.
pub fn signal_term(record: &RunningRecord) -> SignalOutcome {
    signal(&record.identity(), libc::SIGTERM)
}

/// `SIGKILL` the process a record names, if it is still that process.
pub fn signal_kill(record: &RunningRecord) -> SignalOutcome {
    signal(&record.identity(), libc::SIGKILL)
}

/// `SIGTERM` the process `identity` names, if it is still that process.
pub fn signal_term_process(identity: &ProcessIdentity) -> SignalOutcome {
    signal(identity, libc::SIGTERM)
}

/// `SIGKILL` the process `identity` names, and that process alone, never its group, if it is
/// still that process.
pub fn signal_kill_process(identity: &ProcessIdentity) -> SignalOutcome {
    signal(identity, libc::SIGKILL)
}

/// Re-verify layers 1 and 2, then signal — in that order, in this one place.
///
/// A record is a hint, and a pid whose recorded start time no longer matches names a process that
/// inherited the number. Signalling it would end something the operator never launched, so the
/// check is not a caller's to have made earlier: it is run here, immediately before the
/// `kill(2)`, so the window between the two is two adjacent syscalls. No userspace program can
/// close that window; narrowing it to this is the whole of what can be done about it.
#[allow(unsafe_code)]
fn signal(identity: &ProcessIdentity, signal: libc::c_int) -> SignalOutcome {
    if let Some(outcome) = refusal_before_signal(&identity_state(identity)) {
        return outcome;
    }
    // SAFETY: `kill` takes a pid and a signal number by value and dereferences no pointer. The
    // pid is the one the check above just confirmed is held by the process `identity` names, and
    // `pid_is_alive` reads only a pid in `1..=pid_t::MAX` as held, so the cast names that one
    // process and never a group.
    if unsafe { libc::kill(identity.pid as libc::pid_t, signal) } == 0 {
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

/// What a signal to a process in `state` comes to without `kill(2)` being called, or `None` when
/// the process is confirmed as the one that wrote the record and may be signalled.
///
/// A pid whose identity could not be read is refused rather than reported as gone: something holds
/// it, and it may be the capsule.
fn refusal_before_signal(state: &ProcessState) -> Option<SignalOutcome> {
    match state {
        ProcessState::Alive => None,
        ProcessState::Unverified(reason) => Some(SignalOutcome::Refused(reason.clone())),
        ProcessState::Gone(_) => Some(SignalOutcome::AlreadyGone),
    }
}

/// Unlink one record, and its control token file if it has one. For a [`Liveness::Gone`] reading
/// only.
pub fn prune(record: &RunningRecord) {
    if let Ok(dir) = running_dir() {
        let _ = std::fs::remove_file(record_path(&dir, &record.session_id));
        let _ = std::fs::remove_file(token_path(&dir, &record.session_id));
    }
}

/// `~/.murmur/running/<session_id>.control`: where a session declaring `control:` keeps its
/// control token for as long as it runs.
///
/// The `Err` is the running directory being unusable, as [`running_dir`] reports it.
pub fn control_token_path(session_id: &str) -> Result<PathBuf, String> {
    Ok(token_path(&running_dir()?, session_id))
}

/// One session's control token file, written for exactly as long as the session runs.
///
/// A guard of its own rather than part of [`RunningGuard`]: the token is written before the
/// record, and a session whose record could not be written still runs and still holds a token.
pub struct ControlTokenGuard {
    path: PathBuf,
}

impl ControlTokenGuard {
    /// Writes `token` to [`control_token_path`] at owner-only mode and holds it until this guard
    /// drops.
    ///
    /// The `Err` names the path and why; it never carries the token.
    pub fn write(session_id: &str, token: &str) -> Result<Self, String> {
        let path = control_token_path(session_id)?;
        crate::retention::StagedRewrite::stage_with_mode(
            &path,
            token.as_bytes(),
            Some(RECORD_FILE_MODE),
        )
        .and_then(crate::retention::StagedRewrite::commit)
        .map_err(|reason| format!("failed to write {}: {reason}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for ControlTokenGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
        // The record's control token goes with it, whichever guard drops first.
        let _ = std::fs::remove_file(self.path.with_extension(CONTROL_TOKEN_EXTENSION));
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

fn token_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.{CONTROL_TOKEN_EXTENSION}"))
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
///
/// A pid outside `1..=pid_t::MAX` is held by no process. `kill(2)` reads `0` as the caller's own
/// process group and a negative `pid_t` as a process group or every process, so either would
/// otherwise answer for processes the record never named.
#[allow(unsafe_code)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    let Ok(target) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if target == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 runs the existence and permission checks without delivering
    // anything, and dereferences no pointer.
    let held = unsafe { libc::kill(target, 0) } == 0
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
/// | macOS | `proc_bsdinfo.pbi_start_tvsec` / `pbi_start_tvusec`, as `seconds.microseconds` |
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
        start_token_from_stat(&stat)
    }

    /// Field 22 of one `/proc/<pid>/stat` line, or `None` when the line has no `)` or ends before
    /// field 22.
    pub(super) fn start_token_from_stat(stat: &str) -> Option<String> {
        fields_after_comm(stat)?
            .nth(STARTTIME_OFFSET_AFTER_COMM)
            .map(str::to_string)
    }

    /// The fields of a `/proc/<pid>/stat` line from field 3 on, or `None` when it has no `)`.
    fn fields_after_comm(stat: &str) -> Option<std::str::SplitWhitespace<'_>> {
        Some(stat[stat.rfind(')')? + 1..].split_whitespace())
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
        fields_after_comm(&stat).and_then(|mut fields| fields.next()) == Some("Z")
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::mem::size_of;

    /// `proc_bsdinfo.pbi_start_tvsec` and `pbi_start_tvusec`, rendered as `seconds.microseconds`.
    pub(super) fn process_start_token(pid: u32) -> Option<String> {
        let info = bsd_info(pid).ok()?;
        Some(format!(
            "{}.{:06}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }

    /// A process that has exited and not yet been waited on. `kill(pid, 0)` still reports its pid
    /// held, but `PROC_PIDTBSDINFO` refuses to describe it with `ESRCH`; one described mid-exit
    /// carries `pbi_status` `SZOMB`. `ESRCH` read straight after layer 1 found the pid held means
    /// either that or a process that exited in between, and both are gone.
    ///
    /// Any other failed reading, `EPERM` or a short write, is not reported as a zombie, on the
    /// same terms as the Linux reading: layer 2 reads the pid as `Unverified`.
    pub(super) fn is_zombie(pid: u32) -> bool {
        match bsd_info(pid) {
            Ok(info) => info.pbi_status == libc::SZOMB,
            Err(err) => err.raw_os_error() == Some(libc::ESRCH),
        }
    }

    /// The kernel's `PROC_PIDTBSDINFO` description of `pid`. `Err` carries the `errno` when the
    /// call failed, and no OS error when it wrote anything other than exactly one `proc_bsdinfo`.
    #[allow(unsafe_code)]
    fn bsd_info(pid: u32) -> std::io::Result<libc::proc_bsdinfo> {
        // SAFETY: `proc_bsdinfo` is a plain C struct of integers and fixed-size integer arrays,
        // with no pointer, niche or validity invariant, so an all-zero value is a valid one.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = size_of::<libc::proc_bsdinfo>();

        // SAFETY: the buffer is one `proc_bsdinfo` and the size passed is that struct's own size,
        // which bounds what `proc_pidinfo` writes. The kernel copies into it and returns; nothing
        // retains the pointer past the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size as libc::c_int,
            )
        };
        if written <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        if usize::try_from(written).ok() != Some(size) {
            return Err(std::io::Error::other(format!(
                "proc_pidinfo wrote {written} bytes, not one proc_bsdinfo of {size}"
            )));
        }
        Ok(info)
    }
}

/// Layer 3: the session id the door at `record.url` claims on its agent card.
///
/// A record carrying a door token reads the extended card, the only one an authenticated door
/// names its session on, by `agent/getAuthenticatedExtendedCard`; any other record reads the
/// public card. A card with no capsule extension naming a session — one served by a runtime that
/// predates the A2A card, among others — names no session, so its capsule reads as unreachable.
fn probe_session_id(record: &RunningRecord) -> Result<String, String> {
    probe_door_session_id(&record.url, record.door_token.as_ref())
}

/// The session id the door at `url` — `host:port`, with or without `http://` — names on its agent
/// card: the extended card read with `token` when one is given, else the public card.
///
/// The one door probe: [`verify`]'s layer 3 and a formation launcher's readiness check both ask
/// it, so "this door answers as that session" means the same thing to both.
pub(crate) fn probe_door_session_id(
    url: &str,
    token: Option<&crate::door_auth::DoorToken>,
) -> Result<String, String> {
    let addr = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let card = match token {
        Some(token) => {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": crate::identity::DoorMethod::GetAuthenticatedExtendedCard.wire_name(),
                "params": {},
            })
            .to_string();
            let authorization = crate::door_auth::bearer_header(token);
            let response = crate::http_client::http_json_with_timeouts(
                "POST",
                &format!("http://{addr}/"),
                Some(&body),
                &[("Authorization", authorization.as_str())],
                PROBE_CONNECT_TIMEOUT,
                PROBE_READ_TIMEOUT,
            )?;
            response.get("result").cloned().ok_or_else(|| {
                format!("the capsule at {addr} did not return its extended agent card")
            })?
        }
        None => crate::http_client::http_json_with_timeouts(
            "GET",
            &format!("http://{addr}/.well-known/agent-card.json"),
            None,
            &[("Accept", "application/json")],
            PROBE_CONNECT_TIMEOUT,
            PROBE_READ_TIMEOUT,
        )?,
    };
    crate::identity::session_id_from_card(&card)
        .map(str::to_string)
        .ok_or_else(|| format!("the agent card from {addr} names no session"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_removes_the_record_and_its_control_token() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "running::tests::inner_prune_removes_the_record_and_its_control_token",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by prune_removes_the_record_and_its_control_token"]
    fn inner_prune_removes_the_record_and_its_control_token() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let record = RunningRecord {
            session_id: "ses_0199prunecontroltoken000000000000".to_string(),
            url: "localhost:1".to_string(),
            pid: std::process::id(),
            process_start: String::new(),
            capsule_name: "c".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/tmp"),
            outlives_launcher: false,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            credentials: BTreeMap::new(),
            formation_id: None,
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
        };
        let token = ControlTokenGuard::write(&record.session_id, "ctl1.token.mac").unwrap();
        let record_guard = RunningGuard::write(&record).unwrap();
        let dir = running_dir().unwrap();
        let token_file = control_token_path(&record.session_id).unwrap();
        assert_eq!(
            token_file,
            dir.join(format!("{}.control", record.session_id))
        );
        assert_eq!(crate::murmur_home::mode_of(&token_file), RECORD_FILE_MODE);
        assert_eq!(
            std::fs::read_to_string(&token_file).unwrap(),
            "ctl1.token.mac"
        );

        prune(&record);
        assert!(!record_path(&dir, &record.session_id).exists());
        assert!(!token_file.exists());

        // Either guard dropping takes both files.
        std::mem::forget(token);
        std::mem::forget(record_guard);
        let _token = ControlTokenGuard::write(&record.session_id, "ctl1.token.mac").unwrap();
        drop(RunningGuard::write(&record).unwrap());
        assert!(!token_file.exists());
    }

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
            door_token: None,
            credentials: BTreeMap::new(),
            formation_id: None,
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
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

    #[test]
    fn a_record_with_a_door_token_serializes_it_beside_the_nine_fields() {
        let mut with_token = record(1, "42");
        with_token.door_token = Some(crate::door_auth::DoorToken::new(
            "mdt1.payload.mac".to_string(),
        ));
        let value = serde_json::to_value(&with_token).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 10);
        assert_eq!(object["door_token"], "mdt1.payload.mac");
        let back: RunningRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, with_token);
    }

    #[test]
    fn a_record_debug_prints_no_door_token() {
        let mut with_token = record(1, "42");
        with_token.door_token = Some(crate::door_auth::DoorToken::new(
            "mdt1.secretpayload.secretmac".to_string(),
        ));
        let debug = format!("{with_token:?}");
        assert!(!debug.contains("secretpayload"), "{debug}");
        assert!(debug.contains("DoorToken(<redacted>)"), "{debug}");
    }

    fn with_credentials() -> RunningRecord {
        let mut authenticated = record(1, "42");
        authenticated.door_token = Some(crate::door_auth::DoorToken::new(
            "mdt1.operatorpayload.mac".to_string(),
        ));
        for name in ["watcher", "reader"] {
            authenticated.credentials.insert(
                name.to_string(),
                crate::door_auth::DoorToken::new(format!("mdt1.{name}secret.mac")),
            );
        }
        authenticated
    }

    #[test]
    fn a_record_with_credentials_serializes_them_as_an_object_by_name() {
        let authenticated = with_credentials();
        let value = serde_json::to_value(&authenticated).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 11);
        assert_eq!(
            object["credentials"],
            serde_json::json!({
                "reader": "mdt1.readersecret.mac",
                "watcher": "mdt1.watchersecret.mac",
            })
        );
        let back: RunningRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, authenticated);
    }

    #[test]
    fn a_record_debug_prints_no_credential_token() {
        let debug = format!("{:?}", with_credentials());
        assert!(!debug.contains("secret"), "{debug}");
        assert!(!debug.contains("operatorpayload"), "{debug}");
        assert!(
            debug.contains("\"watcher\": DoorToken(<redacted>)"),
            "{debug}"
        );
    }

    /// An operator token alone — a door declaring no credentials — writes no `credentials` key.
    #[test]
    fn a_record_with_no_declared_credentials_omits_the_key() {
        let mut operator_only = record(1, "42");
        operator_only.door_token = Some(crate::door_auth::DoorToken::new(
            "mdt1.payload.mac".to_string(),
        ));
        let value = serde_json::to_value(&operator_only).unwrap();
        assert!(value.get("credentials").is_none(), "{value}");
    }

    /// A record an older runtime wrote has no `credentials` key, and reads as declaring none.
    #[test]
    fn a_record_without_a_credentials_key_parses_as_empty() {
        let mut value = serde_json::to_value(with_credentials()).unwrap();
        value.as_object_mut().unwrap().remove("credentials");
        let parsed: RunningRecord = serde_json::from_value(value).unwrap();
        assert!(parsed.credentials.is_empty());
        assert!(parsed.door_token.is_some());
    }

    /// A record an older runtime wrote has no `door_token` key, and still names a process.
    #[test]
    fn a_record_without_a_door_token_key_parses() {
        let value = serde_json::to_value(record(1, "42")).unwrap();
        assert!(value.get("door_token").is_none());
        let parsed: RunningRecord = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.door_token, None);
    }

    #[test]
    fn a_member_record_serializes_its_formation_beside_the_nine_fields() {
        let formation = crate::formation::FormationId::mint();
        let mut member = record(1, "42");
        member.formation_id = Some(formation.clone());
        let value = serde_json::to_value(&member).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 10);
        assert_eq!(object["formation_id"], formation.as_str());
        let back: RunningRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, member);
    }

    /// A standalone session's record carries no formation key at all, not a null one.
    #[test]
    fn a_standalone_record_has_no_formation_key() {
        let body = serde_json::to_string_pretty(&record(1, "42")).unwrap();
        assert!(!body.contains("formation"), "{body}");
        let parsed: RunningRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.formation_id, None);
    }

    #[test]
    fn a_record_with_a_malformed_formation_id_does_not_parse() {
        let mut value = serde_json::to_value(record(1, "42")).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("formation_id".to_string(), "frm_ABC".into());
        assert!(serde_json::from_value::<RunningRecord>(value).is_err());
    }

    #[test]
    fn list_prunes_a_record_with_a_malformed_formation_id() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "running::tests::inner_list_prunes_a_record_with_a_malformed_formation_id",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by list_prunes_a_record_with_a_malformed_formation_id"]
    fn inner_list_prunes_a_record_with_a_malformed_formation_id() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let dir = running_dir().unwrap();
        let mut member = record(1, "42");
        member.formation_id = Some(crate::formation::FormationId::mint());
        let mut malformed = serde_json::to_value(record(2, "42")).unwrap();
        malformed["session_id"] = "ses_0199c4e2f1b7712a9d3e4f5061728395".into();
        malformed["formation_id"] = "frm_ABC".into();
        let member_path = record_path(&dir, &member.session_id);
        let malformed_path = dir.join("ses_0199c4e2f1b7712a9d3e4f5061728395.json");
        std::fs::write(&member_path, serde_json::to_vec_pretty(&member).unwrap()).unwrap();
        std::fs::write(
            &malformed_path,
            serde_json::to_vec_pretty(&malformed).unwrap(),
        )
        .unwrap();

        assert_eq!(list().unwrap(), vec![member]);
        assert!(member_path.exists());
        assert!(!malformed_path.exists());
    }

    fn launched_member() -> RunningRecord {
        let mut member = record(1, "42");
        member.formation_id = Some(crate::formation::FormationId::mint());
        member.formation_lifeline = true;
        member.formation_launcher = Some(ProcessIdentity {
            pid: 4242,
            process_start: "987654".to_string(),
        });
        member.spawned_by = Some("ses_0199c4e2f1b7712a9d3e4f5061728390".to_string());
        member
    }

    #[test]
    fn a_launched_members_record_round_trips_its_launcher_and_lineage() {
        let member = launched_member();
        let value = serde_json::to_value(&member).unwrap();
        assert_eq!(value["formation_lifeline"], true);
        assert_eq!(
            value["formation_launcher"],
            serde_json::json!({"pid": 4242, "process_start": "987654"})
        );
        assert_eq!(value["spawned_by"], "ses_0199c4e2f1b7712a9d3e4f5061728390");
        let back: RunningRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, member);
    }

    /// The three keys are written only when they apply, so a record without them is byte for
    /// byte the record a standalone session always wrote.
    #[test]
    fn a_record_holding_no_lifeline_launcher_or_spawner_omits_all_three_keys() {
        let mut member = launched_member();
        member.formation_lifeline = false;
        member.formation_launcher = None;
        member.spawned_by = None;
        let body = serde_json::to_string_pretty(&member).unwrap();
        for key in ["formation_lifeline", "formation_launcher", "spawned_by"] {
            assert!(!body.contains(key), "{key} in {body}");
        }
        assert_eq!(
            serde_json::from_str::<RunningRecord>(&body).unwrap(),
            member
        );
    }

    #[test]
    fn a_record_with_a_malformed_formation_launcher_does_not_parse() {
        for launcher in [
            serde_json::json!(4242),
            serde_json::json!({"pid": 4242}),
            serde_json::json!({"process_start": "1"}),
            serde_json::json!({"pid": -1, "process_start": "1"}),
            serde_json::json!({"pid": "4242", "process_start": "1"}),
        ] {
            let mut value = serde_json::to_value(record(1, "42")).unwrap();
            value["formation_launcher"] = launcher.clone();
            assert!(
                serde_json::from_value::<RunningRecord>(value).is_err(),
                "{launcher}"
            );
        }
    }

    /// An identity and a record naming the same pid and start time are one process to layers 1
    /// and 2, and to the signal that re-checks them.
    #[test]
    fn an_identity_reads_and_signals_as_the_record_naming_the_same_process() {
        let pid = std::process::id();
        let token = process_start_token(pid).unwrap();
        for start in [token.as_str(), "not-this-process", ""] {
            let record = record(pid, start);
            let identity = ProcessIdentity {
                pid,
                process_start: start.to_string(),
            };
            assert_eq!(identity_state(&identity), process_state(&record), "{start}");
        }
        // A mismatched start time is the one case safe to signal here: nothing is sent.
        let stale = ProcessIdentity {
            pid,
            process_start: "not-this-process".to_string(),
        };
        assert_eq!(
            signal_term_process(&stale),
            signal_term(&record(pid, "not-this-process"))
        );
        assert_eq!(signal_kill_process(&stale), SignalOutcome::AlreadyGone);
        let nobody = ProcessIdentity {
            pid: 0,
            process_start: "42".to_string(),
        };
        assert_eq!(signal_term_process(&nobody), signal_term(&record(0, "42")));
        assert_eq!(
            ProcessIdentity::of_this_process(),
            Some(ProcessIdentity {
                pid,
                process_start: token
            })
        );
    }

    /// A live process this test owns, signalled through its identity, is sent the signal and goes.
    #[test]
    fn a_term_sent_through_an_identity_reaches_the_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep should start");
        let pid = child.id();
        let identity = ProcessIdentity {
            pid,
            process_start: process_start_token(pid).unwrap(),
        };
        assert_eq!(identity_state(&identity), ProcessState::Alive);
        assert_eq!(signal_term_process(&identity), SignalOutcome::Sent);
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(matches!(identity_state(&identity), ProcessState::Gone(_)));
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
            other => panic!("a mismatched start time must read as gone, not {other:?}"),
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
        // Claimed and never bound: the port is one nothing listens on.
        let port = crate::pinned_port::claim();
        record.url = format!("127.0.0.1:{port}");
        assert!(matches!(verify(&record), Liveness::Unreachable(_)));
    }

    const PID: u32 = 4242;

    #[test]
    fn classify_a_dead_pid_is_gone_whatever_the_token() {
        for token in [None, Some("123"), Some("")] {
            assert_eq!(
                classify(PID, "123", false, token),
                ProcessState::Gone("no process holds pid 4242".to_string()),
                "token {token:?}"
            );
        }
    }

    #[test]
    fn classify_a_live_pid_with_the_recorded_token_is_alive() {
        assert_eq!(classify(PID, "123", true, Some("123")), ProcessState::Alive);
    }

    #[test]
    fn classify_a_live_pid_with_another_token_is_gone() {
        assert_eq!(
            classify(PID, "123", true, Some("456")),
            ProcessState::Gone("pid 4242 is held by a process that started at another time".into())
        );
    }

    /// The writer stores `""` when it could not read its own start time, and that must never match.
    #[test]
    fn classify_a_recorded_empty_token_against_a_fresh_one_is_gone() {
        assert!(matches!(
            classify(PID, "", true, Some("123")),
            ProcessState::Gone(reason) if reason.contains("another time")
        ));
    }

    #[test]
    fn classify_a_live_pid_whose_token_cannot_be_read_is_unverified() {
        assert_eq!(
            classify(PID, "123", true, None),
            ProcessState::Unverified("pid 4242's start time could not be read".into())
        );
    }

    #[test]
    fn nothing_is_refused_before_signalling_a_confirmed_process() {
        assert_eq!(refusal_before_signal(&ProcessState::Alive), None);
    }

    #[test]
    fn a_gone_process_is_already_gone_before_any_signal() {
        assert_eq!(
            refusal_before_signal(&ProcessState::Gone("no process holds pid 1".into())),
            Some(SignalOutcome::AlreadyGone)
        );
    }

    #[test]
    fn an_unverified_process_is_refused_before_any_signal() {
        let reason = "pid 1's start time could not be read".to_string();
        assert_eq!(
            refusal_before_signal(&ProcessState::Unverified(reason.clone())),
            Some(SignalOutcome::Refused(reason))
        );
    }

    /// `liveness_without_probe` has no URL to probe, so an answer for `Unverified` is one reached
    /// without the network.
    #[test]
    fn an_unverified_process_is_unreachable_without_a_probe() {
        let reason = "pid 1's start time could not be read".to_string();
        assert_eq!(
            liveness_without_probe(ProcessState::Unverified(reason.clone())),
            Some(Liveness::Unreachable(reason))
        );
        assert_eq!(
            liveness_without_probe(ProcessState::Gone("gone".into())),
            Some(Liveness::Gone("gone".into()))
        );
        assert_eq!(liveness_without_probe(ProcessState::Alive), None);
    }

    /// A stat line with field 22 set to `987654`: pid, comm, then fields 3 through 22.
    #[cfg(target_os = "linux")]
    fn stat_line(comm: &str) -> String {
        let fields: Vec<String> = (3..=21).map(|field| field.to_string()).collect();
        format!("1234 ({comm}) {} 987654 23 24\n", fields.join(" "))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_start_token_is_field_22_of_a_plain_stat_line() {
        assert_eq!(
            platform::start_token_from_stat(&stat_line("sleep")),
            Some("987654".to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_start_token_is_counted_from_the_last_close_paren() {
        assert_eq!(
            platform::start_token_from_stat(&stat_line("a) (b c")),
            Some("987654".to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_line_with_no_close_paren_has_no_start_token() {
        assert_eq!(platform::start_token_from_stat("1234 sleep S 1 2 3"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stat_line_truncated_before_field_22_has_no_start_token() {
        let line = stat_line("sleep");
        let truncated = &line[..line.find(" 987654").unwrap()];
        assert_eq!(platform::start_token_from_stat(truncated), None);
    }
}

#[cfg(test)]
mod home_tests {
    use super::*;

    #[test]
    fn running_dir_holds_a_wide_murmur_home_owner_only() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::wide_dir(&home.path().join(".murmur"), 0o755);
        crate::murmur_home::run_with_home(
            "running::home_tests::inner_running_dir_under_scratch_home",
            home.path(),
        );
        assert_eq!(
            crate::murmur_home::mode_of(&home.path().join(".murmur")),
            0o700
        );
        assert_eq!(
            crate::murmur_home::mode_of(&home.path().join(".murmur").join(RUNNING_DIR)),
            RUNNING_DIR_MODE
        );
    }

    #[test]
    #[ignore = "run by running_dir_holds_a_wide_murmur_home_owner_only"]
    fn inner_running_dir_under_scratch_home() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        running_dir().unwrap();
    }

    #[test]
    fn running_dir_location_creates_nothing() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "running::home_tests::inner_running_dir_location_creates_nothing",
            home.path(),
        );
        assert!(!home.path().join(".murmur").exists());
    }

    #[test]
    #[ignore = "run by running_dir_location_creates_nothing"]
    fn inner_running_dir_location_creates_nothing() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            running_dir_location().unwrap(),
            home.join(".murmur").join(RUNNING_DIR)
        );
        assert!(!home.join(".murmur").exists());
    }

    fn record(session_id: &str) -> RunningRecord {
        RunningRecord {
            session_id: session_id.to_string(),
            url: "127.0.0.1:1".to_string(),
            pid: 1,
            process_start: "42".to_string(),
            capsule_name: "demo".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/tmp/demo"),
            outlives_launcher: true,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            credentials: BTreeMap::new(),
            formation_id: None,
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
        }
    }

    #[test]
    fn scan_reports_an_unparseable_file_and_leaves_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let older = record("ses_0199c4e2f1b7712a9d3e4f5061728390");
        let newer = record("ses_0199c4e2f1b7712a9d3e4f5061728391");
        for record in [&older, &newer] {
            std::fs::write(
                record_path(dir.path(), &record.session_id),
                serde_json::to_vec(record).unwrap(),
            )
            .unwrap();
        }
        let broken = dir.path().join("ses_broken.json");
        std::fs::write(&broken, "{").unwrap();
        let staging = dir.path().join(".ses_x.json.1.2.tmp");
        std::fs::write(&staging, "{").unwrap();
        let control = token_path(dir.path(), &older.session_id);
        std::fs::write(&control, "ctl1.token.mac").unwrap();

        let scan = scan(dir.path()).unwrap();

        assert_eq!(scan.records, vec![newer, older]);
        assert_eq!(scan.unreadable, vec![broken.clone()]);
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "{");
        assert!(staging.exists());
        assert!(control.exists());
    }

    #[test]
    fn scan_of_a_missing_directory_is_empty_and_creates_nothing() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join(".murmur").join(RUNNING_DIR);

        let scan = scan(&dir).unwrap();

        assert!(scan.records.is_empty());
        assert!(scan.unreadable.is_empty());
        assert!(!parent.path().join(".murmur").exists());
    }

    #[test]
    fn scan_of_a_regular_file_errs_naming_it() {
        let parent = tempfile::tempdir().unwrap();
        let file = parent.path().join(RUNNING_DIR);
        std::fs::write(&file, "").unwrap();

        let err = scan(&file).unwrap_err();

        assert!(err.starts_with(&format!("{}: ", file.display())), "{err}");
    }
}
