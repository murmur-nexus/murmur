//! Launching a formation: every member of an admitted roster as a `mur run` subprocess of its own,
//! peers first and the entry member last, and every member stopped once the entry member's process
//! has ended.
//!
//! **The launcher supervises; it runs no session.** Each member, the entry member included, is a
//! `mur run --capsule <capsule> --capsule-version <version>` process this module starts and owns.
//! The launcher waits on the entry member's process, so the peers are stopped however the entry
//! member ends — its task finishing, its own `exit(143)`, or a `SIGKILL` of its process.
//!
//! **Every member holds a lifeline.** Each member is handed the read end of a pipe of its own
//! ([`crate::lifeline`]), whose write end only the launcher holds and never writes to. A member
//! that reads EOF winds down as a first `SIGTERM` would wind it down. The launcher ends a
//! formation by closing lifelines; when the launcher itself dies — `SIGKILL` and the OOM killer
//! included — the kernel closes them, so no member outlives its formation.
//!
//! **Ready means the door answers as the session that reported it.** A peer is ready when the door
//! at the URL its readiness line reported serves an agent card naming the session id that line
//! reported, read through [`crate::running`]'s door probe. A live pid proves nothing, and a door
//! some other process holds on the reported port names another session. Every member has one
//! deadline, counted from its spawn.
//!
//! **All or nothing.** A peer that fails to come up refuses the whole launch: every member already
//! started is stopped and reaped before the refusal is returned, and the entry member is never
//! started.
//!
//! **The launcher is the formation's one principal.** It generates the formation's signing key
//! ([`FormationAuthority`]) after the id and holds it in memory alone. Each member gets a pipe of
//! its own, whose read end only that member's `mur run` inherits, named by
//! [`FORMATION_CHANNEL_ENV`]. Its first line — written before the member is spawned — carries the
//! formation id, the member's name, the verification key and one token per member it may call.
//! Once every peer's door answers, each peer that calls other peers is written one address line
//! naming their doors; the entry member's first line and address line are both written before it is
//! spawned. Nothing here writes a token to stdout, stderr, a file, argv or an environment
//! variable. Callees never include the entry member, which runs `task_acceptance: single` busy
//! with the formation's own task and so can serve no peer; such an edge is reported by
//! [`unserved_edges`]. The key and the write ends are dropped once every member is reaped.
//!
//! **Teardown.**
//!
//! * One owner holds every started member, and every return path stops and reaps them. Its `Drop`
//!   does the same, so an early return or an unwinding panic leaves nothing running.
//! * Stopping the members is closing every lifeline, up to [`MEMBER_STOP_GRACE`] for each to end
//!   in order, `SIGKILL` to the process group of any still running, and a reap. The launcher sends
//!   no member `SIGTERM`. Members are waited on concurrently.
//! * Every lifeline is created before the first member is spawned, and the launcher spawns
//!   nothing but members, so no process but the launcher ever holds a write end.
//! * Peers lead process groups of their own, so a terminal's `^C` reaches only the launcher and
//!   the entry member, and the launcher stops the peers in order. The entry member stays in the
//!   launcher's group and behaves at a terminal as a hand-run `mur run` does.
//! * Under `panic = "abort"` no destructor runs, so a panic hook — installed once per process —
//!   `SIGKILL`s every registered member's process group before the abort. It reads a lock-free
//!   registry of member pids, never a mutex a panicking thread might hold.
//! * [`catch_launcher_signals`] turns `SIGINT`, `SIGTERM` and `SIGHUP` into a flag every wait here
//!   polls, so a signalled launcher stops its members — the entry member through its lifeline,
//!   like every other — before it exits.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::sync::{mpsc, Once};
use std::time::{Duration, Instant};

use crate::child_launch::{self, AfterReadiness, ProcessEnv, ProcessLaunch, StartedProcess};
use crate::delegation::SPAWNER_ENV;
use crate::door_auth::DoorToken;
use crate::formation::{FormationId, FormationPeer, FORMATION_PEERS_ENV};
use crate::formation_credentials::{
    render_address_line, FormationAuthority, FORMATION_CHANNEL_ENV,
};
use crate::roster::{AdmittedRoster, RosterEdge};

/// How long a member has, from its spawn, to report itself and have its door answer.
///
/// The bound a delegated child gets for its readiness line, for the same reason: it bounds one
/// `mur run` subprocess becoming usable on a cold or loaded host, where compiling the driver
/// component is the slowest step.
pub const MEMBER_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// How long a member has, after its lifeline is closed, to end in order before its process group
/// is killed.
///
/// Longer than the runtime's own teardown deadline, so a member whose session is ending in order
/// finishes before it is killed and its trace stays readable.
pub const MEMBER_STOP_GRACE: Duration = Duration::from_secs(25);

/// How often a peer's door is probed while it is not yet ready.
const READY_PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How often a wait here looks at a member's process, at the launcher's signal flag, and at
/// whether another peer has already failed.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a member whose stdout has closed is given to exit on its own before it is ended.
const EXIT_AFTER_EOF_WAIT: Duration = Duration::from_secs(2);

/// The address every peer binds its door on. Members of one formation share a host.
pub const PEER_BIND_ADDR: &str = "127.0.0.1";

/// The hidden `mur run` flag carrying the sha256 admission bound a member to.
pub const CAPSULE_SHA256_FLAG: &str = "--capsule-sha256";

/// The hidden `mur run` flag that keeps a peer from running a `task.md` it finds in the shared
/// project directory.
pub const IGNORE_TASK_FILE_FLAG: &str = "--ignore-task-file";

/// What a formation launch is given besides its roster.
#[derive(Debug, Clone)]
pub struct FormationLaunchOptions {
    /// The id every member is launched with, minted once for this launch.
    pub formation_id: FormationId,
    /// The roster's project directory: every member's `--workdir`, so all of them resolve from
    /// the stores admission read.
    pub project_dir: PathBuf,
    /// `--task`, passed to the entry member only.
    pub task: Option<String>,
    /// `--json`, passed to the entry member only; peers always run with it.
    pub json: bool,
    /// `--verbose`, passed to the entry member only.
    pub verbose: bool,
    /// `--no-env-file`, passed to every member.
    pub no_env_file: bool,
    /// `--containment <class>`, passed to every member.
    pub containment: Option<String>,
    /// Each member's deadline to become ready, from its spawn. [`MEMBER_READY_TIMEOUT`] in
    /// production.
    pub ready_timeout: Duration,
    /// How long a stopped member has after its lifeline is closed. [`MEMBER_STOP_GRACE`] in
    /// production.
    pub stop_grace: Duration,
}

impl FormationLaunchOptions {
    /// Options with the production deadline and grace, and nothing passed through.
    pub fn new(formation_id: FormationId, project_dir: PathBuf) -> Self {
        Self {
            formation_id,
            project_dir,
            task: None,
            json: false,
            verbose: false,
            no_env_file: false,
            containment: None,
            ready_timeout: MEMBER_READY_TIMEOUT,
            stop_grace: MEMBER_STOP_GRACE,
        }
    }
}

/// One member as the launcher starts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedMember {
    pub(crate) name: String,
    pub(crate) capsule: String,
    pub(crate) version: String,
    /// The sha256 admission bound the member to, handed to its `mur run` to check.
    pub(crate) sha256: String,
}

/// What a launch starts: the peers in roster order, the entry member, and the members each member
/// may call.
#[derive(Debug, Clone)]
pub(crate) struct FormationPlan {
    pub(crate) peers: Vec<PlannedMember>,
    pub(crate) entry: PlannedMember,
    /// Every member's callees in roster order, the entry member never among them. A member with
    /// none is absent.
    pub(crate) callees: Vec<(String, Vec<String>)>,
}

impl FormationPlan {
    pub(crate) fn from_roster(roster: &AdmittedRoster) -> Self {
        let planned = |member: &crate::roster::AdmittedMember| PlannedMember {
            name: member.name.clone(),
            capsule: member.capsule.clone(),
            version: member.version.clone(),
            sha256: member.sha256.clone(),
        };
        let entry = roster.entry();
        Self {
            peers: roster
                .members()
                .iter()
                .filter(|member| !member.entry)
                .map(planned)
                .collect(),
            entry: planned(entry),
            callees: roster
                .members()
                .iter()
                .map(|member| {
                    let callees: Vec<String> = roster
                        .callees(&member.name)
                        .filter(|callee| !callee.entry)
                        .map(|callee| callee.name.clone())
                        .collect();
                    (member.name.clone(), callees)
                })
                .filter(|(_, callees)| !callees.is_empty())
                .collect(),
        }
    }

    /// The members `member` may call, in roster order.
    pub(crate) fn callees_of(&self, member: &str) -> &[String] {
        self.callees
            .iter()
            .find(|(name, _)| name == member)
            .map(|(_, callees)| callees.as_slice())
            .unwrap_or_default()
    }

    /// `member`'s channel first line, signed by `authority`.
    fn first_line(&self, authority: &FormationAuthority, member: &str) -> String {
        let callees: Vec<&str> = self.callees_of(member).iter().map(String::as_str).collect();
        authority.member_bundle(member, &callees).render_line()
    }
}

/// Every roster edge into the entry member, in roster order: none is served. The entry member runs
/// `task_acceptance: single` and is busy with the formation's own task from launch until it exits,
/// so it can never accept a peer's task, and the member at the edge's other end is handed no
/// credential or address for it.
pub fn unserved_edges(roster: &AdmittedRoster) -> Vec<&RosterEdge> {
    let entry = &roster.entry().name;
    roster
        .edges()
        .iter()
        .filter(|edge| &edge.to == entry)
        .collect()
}

/// A peer whose door answered as the session its readiness line named.
pub struct ReadyPeer {
    pub name: String,
    pub capsule: String,
    pub version: String,
    pub pid: u32,
    pub session_id: String,
    /// `http://host:port`.
    pub url: String,
    /// The operator token from the peer's readiness line, present when the peer declares
    /// `network.authentication`. Held for the readiness probe and never printed.
    door_token: Option<DoorToken>,
}

impl ReadyPeer {
    /// The operator token for this peer's door, or `None` for a public door.
    pub fn door_token(&self) -> Option<&DoorToken> {
        self.door_token.as_ref()
    }
}

impl std::fmt::Debug for ReadyPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyPeer")
            .field("name", &self.name)
            .field("capsule", &self.capsule)
            .field("version", &self.version)
            .field("pid", &self.pid)
            .field("session_id", &self.session_id)
            .field("url", &self.url)
            .field(
                "door_token",
                &self.door_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Why one member did not come up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberFailureReason {
    /// Its process could not be started.
    NotStarted { error: String },
    /// Its process ended before it printed a readiness line.
    ExitedBeforeReporting {
        status: String,
        stderr_tail: Vec<String>,
    },
    /// Its process reported and then ended before its door answered.
    ExitedBeforeDoorAnswered {
        status: String,
        stderr_tail: Vec<String>,
    },
    /// No readiness line within the deadline.
    DidNotReport { deadline: Duration },
    /// The first line it printed is not a readiness line.
    UnreadableReport { why: String },
    /// Its readiness line names no door to probe.
    ReportedNoDoor,
    /// Its readiness line names another formation, or none.
    WrongFormation {
        reported: Option<String>,
        expected: String,
    },
    /// Its door never answered as the session it reported, within the deadline.
    DoorDidNotAnswer {
        url: String,
        session_id: String,
        deadline: Duration,
        last_error: String,
    },
}

impl std::fmt::Display for MemberFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tail = |f: &mut std::fmt::Formatter<'_>, lines: &[String]| -> std::fmt::Result {
            if lines.is_empty() {
                return f.write_str("; it wrote nothing to stderr");
            }
            f.write_str("; its last stderr lines were:")?;
            for line in lines {
                write!(f, "\n      {line}")?;
            }
            Ok(())
        };
        match self {
            Self::NotStarted { error } => write!(f, "its process could not be started: {error}"),
            Self::ExitedBeforeReporting {
                status,
                stderr_tail,
            } => {
                write!(f, "its process exited with {status} before reporting")?;
                tail(f, stderr_tail)
            }
            Self::ExitedBeforeDoorAnswered {
                status,
                stderr_tail,
            } => {
                write!(
                    f,
                    "its process reported and then exited with {status} before its door answered"
                )?;
                tail(f, stderr_tail)
            }
            Self::DidNotReport { deadline } => {
                write!(f, "it did not report within {}s", deadline.as_secs_f32())
            }
            Self::UnreadableReport { why } => {
                write!(f, "its first line is not a readiness line: {why}")
            }
            Self::ReportedNoDoor => f.write_str("it reported no door"),
            Self::WrongFormation { reported, expected } => match reported {
                Some(reported) => {
                    write!(f, "it reported formation {reported} instead of {expected}")
                }
                None => write!(f, "it reported no formation instead of {expected}"),
            },
            Self::DoorDidNotAnswer {
                url,
                session_id,
                deadline,
                last_error,
            } => write!(
                f,
                "its door at {url} did not answer as session {session_id} within {}s; the last \
                 probe said: {last_error}",
                deadline.as_secs_f32()
            ),
        }
    }
}

/// One member that did not come up, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFailure {
    pub name: String,
    pub capsule: String,
    pub version: String,
    pub reason: MemberFailureReason,
}

/// A member whose process was still running when the launcher stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedMember {
    pub name: String,
    pub pid: u32,
}

/// A launch that did not complete. By the time this is returned, nothing it started is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberLaunchFailure {
    /// Every member known to have failed, in roster order. Empty when the launch was interrupted
    /// by a signal before any member failed.
    pub failed: Vec<MemberFailure>,
    /// Every member that had started and was stopped, in roster order.
    pub stopped: Vec<StoppedMember>,
    /// The signal the launcher caught while members were coming up, when that is what ended the
    /// launch.
    pub interrupted_by: Option<i32>,
}

impl std::fmt::Display for MemberLaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.interrupted_by {
            Some(signal) if self.failed.is_empty() => write!(
                f,
                "the formation launch was interrupted by {} before every member was ready",
                signal_name(signal)
            )?,
            _ => f.write_str("the formation could not be launched")?,
        }
        for failure in &self.failed {
            write!(
                f,
                "\n  '{}' ({}@{}): {}",
                failure.name, failure.capsule, failure.version, failure.reason
            )?;
        }
        write!(f, "\n  stopped: {}", render_stopped(&self.stopped))
    }
}

/// `name (pid N), …`, or `none`.
pub fn render_stopped(stopped: &[StoppedMember]) -> String {
    if stopped.is_empty() {
        return "none".to_string();
    }
    stopped
        .iter()
        .map(|member| format!("{} (pid {})", member.name, member.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The conventional name of the three signals a launcher catches.
pub fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGTERM => "SIGTERM".to_string(),
        libc::SIGHUP => "SIGHUP".to_string(),
        other => format!("signal {other}"),
    }
}

/// How the entry member's process, and with it the formation, ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormationEnding {
    /// The entry member's process ended on its own, or by a signal the launcher did not send.
    EntryExited(ExitStatus),
    /// The launcher caught this signal and stopped the formation, the entry member included.
    Signalled(i32),
}

/// A formation that has ended, every member stopped and reaped.
#[derive(Debug, Clone)]
pub struct FormationExit {
    pub ending: FormationEnding,
    /// Every member still running when the formation was stopped, in roster order.
    pub stopped: Vec<StoppedMember>,
}

impl FormationExit {
    /// The launcher's exit status: the entry member's code, or 128 plus the signal that ended it,
    /// or 128 plus the signal the launcher caught.
    pub fn exit_code(&self) -> i32 {
        match &self.ending {
            FormationEnding::EntryExited(status) => crate::shell::exit_code_of(status),
            FormationEnding::Signalled(signal) => 128 + signal,
        }
    }
}

/// `status N` or `signal N`, for a refusal.
fn describe_status(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("status {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    status.to_string()
}

// ── The launcher's signals ────────────────────────────────────────────────────

/// The signal the launcher caught, or 0. Written only by [`record_signal`].
static CAUGHT_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn record_signal(signal: libc::c_int) {
    // An atomic store is async-signal-safe; nothing else happens in the handler.
    CAUGHT_SIGNAL.store(signal, Ordering::SeqCst);
}

/// Catch `SIGINT`, `SIGTERM` and `SIGHUP` for the rest of this process's life, so a formation
/// launcher stops its members before it exits rather than dying with them running.
///
/// Every wait in this module polls the flag the handler sets. Called by the launcher only: a test
/// harness that launched formations in-process would otherwise stop answering `^C`.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn catch_launcher_signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `sigaction` is given a zeroed, then fully initialised, struct whose handler is
        // an `extern "C"` function that only stores to an atomic, which is async-signal-safe. The
        // old-action pointer is null, so nothing is written back.
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        if installed != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn catch_launcher_signals() -> std::io::Result<()> {
    Ok(())
}

/// The signal [`catch_launcher_signals`] caught, if any.
pub fn caught_signal() -> Option<i32> {
    match CAUGHT_SIGNAL.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

// ── The member registry the panic hook reads ──────────────────────────────────

/// How many members one process can have registered at once. A slot that cannot be had leaves
/// that member to the owner's `Drop` alone; nothing else depends on the registry.
const REGISTRY_SLOTS: usize = 1024;

/// Every running member's pid: positive for a member leading its own process group, which the
/// hook kills as a group; negative for the entry member, which shares the launcher's group and is
/// killed alone; 0 for a free slot.
static MEMBER_REGISTRY: [AtomicI64; REGISTRY_SLOTS] = [const { AtomicI64::new(0) }; REGISTRY_SLOTS];

/// One registered member. Releasing it — on reap, or on drop — frees the slot.
struct RegistrySlot(Option<usize>);

impl RegistrySlot {
    fn register(pid: u32, own_group: bool) -> Self {
        let pid = i64::from(pid);
        let value = if own_group { pid } else { -pid };
        let slot = MEMBER_REGISTRY.iter().position(|slot| {
            slot.compare_exchange(0, value, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });
        Self(slot)
    }

    fn release(&mut self) {
        if let Some(index) = self.0.take() {
            MEMBER_REGISTRY[index].store(0, Ordering::SeqCst);
        }
    }
}

impl Drop for RegistrySlot {
    fn drop(&mut self) {
        self.release();
    }
}

/// `SIGKILL` every registered member: a member leading its own group as a group, the entry member
/// alone. Reads atomics only, so a panic hook can call it whatever lock the panicking thread holds.
pub(crate) fn kill_registered_members() {
    for slot in &MEMBER_REGISTRY {
        match slot.load(Ordering::SeqCst) {
            0 => {}
            pid if pid > 0 => signal_group(pid, libc::SIGKILL),
            pid => signal_pid(-pid, libc::SIGKILL),
        }
    }
}

/// Install, once per process, the hook that kills every registered member before a
/// `panic = "abort"` build aborts. An unwinding build runs the owner's `Drop` instead, which stops
/// members in order.
fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if cfg!(panic = "abort") {
                kill_registered_members();
            }
        }));
    });
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn signal_pid(pid: i64, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 0 {
        return;
    }
    // SAFETY: `kill` takes two integers and dereferences nothing. `pid` is positive, so it names
    // one process, never a group or every process.
    unsafe {
        libc::kill(pid, signal);
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn signal_group(pgid: i64, signal: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    if pgid <= 1 {
        return;
    }
    // SAFETY: `killpg` takes two integers and dereferences nothing. `pgid` is greater than 1, so
    // it names one process group, never the caller's own or every process.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

#[cfg(not(unix))]
fn signal_pid(_pid: i64, _signal: i32) {}

#[cfg(not(unix))]
fn signal_group(_pgid: i64, _signal: i32) {}

// ── One member's process ──────────────────────────────────────────────────────

/// A started member's process, owned here and nowhere else.
struct MemberProcess {
    name: String,
    pid: u32,
    child: Child,
    own_group: bool,
    /// The launcher's side of the member's lifeline: the write end, open until the member is
    /// stopped.
    #[cfg(unix)]
    lifeline: Option<crate::lifeline::MemberLifeline>,
    /// The exit status, once reaped. A reaped pid may already belong to another process, so
    /// nothing signals or probes it after this is set.
    status: Option<ExitStatus>,
    slot: RegistrySlot,
    grace: Duration,
    /// The write end of the member's formation channel, kept open while the member runs so an
    /// address line can follow its first line, and closed once it is reaped.
    channel: Option<std::io::PipeWriter>,
}

impl MemberProcess {
    fn new(
        name: &str,
        child: Child,
        own_group: bool,
        grace: Duration,
        channel: std::io::PipeWriter,
        #[cfg(unix)] lifeline: Option<crate::lifeline::MemberLifeline>,
    ) -> Self {
        let pid = child.id();
        Self {
            name: name.to_string(),
            pid,
            child,
            own_group,
            #[cfg(unix)]
            lifeline,
            status: None,
            slot: RegistrySlot::register(pid, own_group),
            grace,
            channel: Some(channel),
        }
    }

    /// Write one line to the member's channel. A member that has already gone has closed its end;
    /// the line then reaches nobody, which is what it is for.
    fn send_line(&mut self, line: &str) {
        if let Some(channel) = self.channel.as_mut() {
            let _ = channel.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// Whether the process has ended, reaped or not.
    fn has_exited(&self) -> bool {
        self.status.is_some() || !crate::running::pid_is_alive(self.pid)
    }

    /// The exit status, reaping the process if it has ended. `None` while it runs.
    ///
    /// A member leading its own group has the group killed first, while its unreaped process
    /// still holds the group id, so nothing it started outlives it and no recycled id is signalled.
    fn exit_status_now(&mut self) -> Option<ExitStatus> {
        if self.status.is_none() && self.has_exited() {
            self.reap();
        }
        self.status
    }

    fn reap(&mut self) {
        if self.status.is_some() {
            return;
        }
        if self.own_group {
            signal_group(i64::from(self.pid), libc::SIGKILL);
        } else if !self.has_exited() {
            signal_pid(i64::from(self.pid), libc::SIGKILL);
        }
        self.status = self.child.wait().ok();
        if self.status.is_none() {
            // `wait` failing leaves nothing further to ask; the process is not ours to probe.
            self.status = Some(ExitStatus::default());
        }
        self.slot.release();
        self.channel = None;
    }

    /// Close the member's lifeline: it reads EOF and winds down. Idempotent.
    fn close_lifeline(&mut self) {
        #[cfg(unix)]
        if let Some(lifeline) = &mut self.lifeline {
            lifeline.close();
        }
    }

    /// Whether the process is running and not yet reaped.
    fn is_running(&self) -> bool {
        self.status.is_none() && !self.has_exited()
    }

    /// Close its lifeline, up to the grace for it to end, `SIGKILL` to its group, and a reap.
    /// Idempotent. `Some` when the process was still running when the stop began.
    fn stop(&mut self) -> Option<StoppedMember> {
        let was_running = self.is_running();
        self.stop_from(was_running)
    }

    /// [`Self::stop`], with whether the process was running when the stop began read by the
    /// caller: a member can end on its lifeline's EOF the moment that lifeline closes.
    fn stop_from(&mut self, was_running: bool) -> Option<StoppedMember> {
        if self.status.is_some() {
            return None;
        }
        self.close_lifeline();
        if was_running {
            let deadline = Instant::now() + self.grace;
            while !self.has_exited() && Instant::now() < deadline {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
        self.reap();
        was_running.then(|| StoppedMember {
            name: self.name.clone(),
            pid: self.pid,
        })
    }
}

impl Drop for MemberProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Every started member, in roster order: the one owner, and what every exit path stops.
struct MemberSet(Vec<MemberProcess>);

impl MemberSet {
    /// Close every member's lifeline, then wait on, kill if need be, and reap every member
    /// concurrently. Idempotent.
    fn stop_all(&mut self) -> Vec<StoppedMember> {
        // Read before any lifeline closes: a member that ends on its EOF while another lifeline is
        // still being closed was running when the stop began, and is reported as stopped.
        let running: Vec<bool> = self.0.iter().map(MemberProcess::is_running).collect();
        for member in &mut self.0 {
            member.close_lifeline();
        }
        std::thread::scope(|scope| {
            let stopping: Vec<_> = self
                .0
                .iter_mut()
                .zip(running)
                .map(|(member, was_running)| scope.spawn(move || member.stop_from(was_running)))
                .collect();
            stopping
                .into_iter()
                .filter_map(|handle| handle.join().ok().flatten())
                .collect()
        })
    }
}

impl Drop for MemberSet {
    fn drop(&mut self) {
        let _ = self.stop_all();
    }
}

// ── Starting members ──────────────────────────────────────────────────────────

/// The arguments every member's `mur run` starts with.
fn member_args(member: &PlannedMember, options: &FormationLaunchOptions) -> Vec<String> {
    vec![
        "run".to_string(),
        "--capsule".to_string(),
        member.capsule.clone(),
        "--capsule-version".to_string(),
        member.version.clone(),
        CAPSULE_SHA256_FLAG.to_string(),
        member.sha256.clone(),
        "--workdir".to_string(),
        options.project_dir.display().to_string(),
    ]
}

/// The pass-through flags every member gets.
fn pass_through(args: &mut Vec<String>, options: &FormationLaunchOptions) {
    if options.no_env_file {
        args.push("--no-env-file".to_string());
    }
    if let Some(class) = &options.containment {
        args.push("--containment".to_string());
        args.push(class.clone());
    }
}

/// A peer's arguments after the binary.
pub(crate) fn peer_args(member: &PlannedMember, options: &FormationLaunchOptions) -> Vec<String> {
    let mut args = member_args(member, options);
    args.extend([
        "--json".to_string(),
        "--bind".to_string(),
        PEER_BIND_ADDR.to_string(),
        // Every member's workdir is the project directory, where the entry member's task is
        // written; a peer takes work only at its door.
        IGNORE_TASK_FILE_FLAG.to_string(),
    ]);
    pass_through(&mut args, options);
    args
}

/// The entry member's arguments after the binary.
pub(crate) fn entry_args(member: &PlannedMember, options: &FormationLaunchOptions) -> Vec<String> {
    let mut args = member_args(member, options);
    args.extend([
        "--lifecycle-task-acceptance".to_string(),
        "single".to_string(),
        "--lifecycle-after-task".to_string(),
        "exit".to_string(),
    ]);
    if let Some(task) = &options.task {
        args.push("--task".to_string());
        args.push(task.clone());
    }
    if options.json {
        args.push("--json".to_string());
    }
    if options.verbose {
        args.push("--verbose".to_string());
    }
    pass_through(&mut args, options);
    args
}

/// What became of one peer's start.
enum PeerOutcome {
    Ready(ReadyPeer),
    Failed(MemberFailureReason),
    /// Given up on because another peer failed or the launcher was signalled.
    Abandoned,
}

/// A member's formation channel: the write end the launcher keeps, already holding the member's
/// first line, and the read end the member's process inherits.
struct Channel {
    writer: std::io::PipeWriter,
    reader: std::io::PipeReader,
}

impl Channel {
    /// A fresh pipe with `first_line` already written into it. Both ends are close-on-exec, so no
    /// process inherits either unless [`ProcessLaunch::inherit_fd`] names it.
    fn open(first_line: &str) -> Result<Self, MemberFailureReason> {
        let (reader, mut writer) = std::io::pipe().map_err(|error| {
            not_started(&format!(
                "its formation channel could not be created: {error}"
            ))
        })?;
        writer
            .write_all(format!("{first_line}\n").as_bytes())
            .map_err(|error| {
                not_started(&format!(
                    "its formation channel could not be written: {error}"
                ))
            })?;
        Ok(Self { writer, reader })
    }

    /// The descriptor number the member reads its channel from.
    fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.reader.as_raw_fd()
    }

    /// The environment every member is started with: its formation id and its channel.
    fn member_env(&self, formation_id: &FormationId) -> Vec<(String, String)> {
        let (id_name, id_value) = formation_id.env_pair();
        vec![
            (id_name.to_string(), id_value),
            (FORMATION_CHANNEL_ENV.to_string(), self.fd().to_string()),
        ]
    }
}

/// Start one peer and wait until its door answers, it fails, or `abort` is raised.
///
/// The process, when one was started, is returned whatever the outcome, so the caller owns
/// stopping it.
fn start_peer(
    binary: &Path,
    member: &PlannedMember,
    first_line: &str,
    options: &FormationLaunchOptions,
    abort: &AtomicBool,
    #[cfg(unix)] mut lifeline: crate::lifeline::MemberLifeline,
) -> (Option<MemberProcess>, PeerOutcome) {
    let args = peer_args(member, options);
    let prefix = format!("[{}] ", member.name);
    let channel = match Channel::open(first_line) {
        Ok(channel) => channel,
        Err(reason) => return (None, PeerOutcome::Failed(reason)),
    };
    let set = channel.member_env(&options.formation_id);
    let spawned_at = Instant::now();
    let deadline = spawned_at + options.ready_timeout;
    let started = child_launch::start_process(&ProcessLaunch {
        binary,
        args: &args,
        cwd: None,
        env: ProcessEnv::Inherited {
            set: &set,
            remove: &[FORMATION_PEERS_ENV, SPAWNER_ENV],
        },
        stdin_line: None,
        stderr_prefix: Some(&prefix),
        inherit_output: false,
        own_process_group: true,
        inherit_fd: Some(channel.fd()),
        #[cfg(unix)]
        lifeline: Some(&lifeline),
    });
    // The member holds its own copy of the read end now, or never will.
    let Channel { writer, reader } = channel;
    drop(reader);
    let StartedProcess {
        mut child,
        stderr_tail,
        ..
    } = match started {
        Ok(started) => started,
        Err(error) => {
            return (
                None,
                PeerOutcome::Failed(MemberFailureReason::NotStarted {
                    error: format!("{}: {error}", binary.display()),
                }),
            )
        }
    };
    #[cfg(unix)]
    lifeline.spawned();
    let stdout = child.stdout.take();
    let mut process = MemberProcess::new(
        &member.name,
        child,
        true,
        options.stop_grace,
        writer,
        #[cfg(unix)]
        Some(lifeline),
    );
    let Some(stdout) = stdout else {
        return (
            Some(process),
            PeerOutcome::Failed(MemberFailureReason::NotStarted {
                error: "the process exposed no standard output".to_string(),
            }),
        );
    };
    let lines = child_launch::relay_readiness_line(stdout, AfterReadiness::PrefixedStderr(prefix));

    // The readiness line.
    let line = loop {
        if abort.load(Ordering::SeqCst) {
            return (Some(process), PeerOutcome::Abandoned);
        }
        let now = Instant::now();
        if now >= deadline {
            return (
                Some(process),
                PeerOutcome::Failed(MemberFailureReason::DidNotReport {
                    deadline: options.ready_timeout,
                }),
            );
        }
        match lines.recv_timeout(POLL_INTERVAL.min(deadline - now)) {
            Ok(Some(line)) => break line,
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Its stdout closes as it exits, so the exit itself is at most a moment behind. A
                // process that closed stdout and stayed up is as good as gone, and is ended.
                let closed_at = Instant::now();
                let status = loop {
                    if let Some(status) = process.exit_status_now() {
                        break status;
                    }
                    if closed_at.elapsed() >= EXIT_AFTER_EOF_WAIT {
                        process.reap();
                        break process.status.unwrap_or_default();
                    }
                    std::thread::sleep(POLL_INTERVAL);
                };
                return (
                    Some(process),
                    PeerOutcome::Failed(MemberFailureReason::ExitedBeforeReporting {
                        status: describe_status(status),
                        stderr_tail: stderr_tail.lines_at_end(),
                    }),
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };

    let report = match read_report(&line, &options.formation_id) {
        Ok(report) => report,
        Err(reason) => return (Some(process), PeerOutcome::Failed(reason)),
    };

    // The door.
    let mut last_error;
    loop {
        if abort.load(Ordering::SeqCst) {
            return (Some(process), PeerOutcome::Abandoned);
        }
        if let Some(status) = process.exit_status_now() {
            return (
                Some(process),
                PeerOutcome::Failed(MemberFailureReason::ExitedBeforeDoorAnswered {
                    status: describe_status(status),
                    stderr_tail: stderr_tail.lines_at_end(),
                }),
            );
        }
        match crate::running::probe_door_session_id(&report.url, report.door_token.as_ref()) {
            Ok(session_id) if session_id == report.session_id => {
                let pid = process.pid;
                return (
                    Some(process),
                    PeerOutcome::Ready(ReadyPeer {
                        name: member.name.clone(),
                        capsule: member.capsule.clone(),
                        version: member.version.clone(),
                        pid,
                        session_id: report.session_id,
                        url: report.url,
                        door_token: report.door_token,
                    }),
                );
            }
            Ok(other) => last_error = format!("the door answers as session {other}"),
            Err(error) => last_error = error,
        }
        let now = Instant::now();
        if now >= deadline {
            return (
                Some(process),
                PeerOutcome::Failed(MemberFailureReason::DoorDidNotAnswer {
                    url: report.url,
                    session_id: report.session_id,
                    deadline: options.ready_timeout,
                    last_error,
                }),
            );
        }
        std::thread::sleep(READY_PROBE_INTERVAL.min(deadline - now));
    }
}

/// What a peer's readiness line says about its door.
struct Report {
    session_id: String,
    /// `http://host:port`.
    url: String,
    door_token: Option<DoorToken>,
}

/// Read a readiness line, refusing one that names another formation or no door.
fn read_report(line: &str, expected: &FormationId) -> Result<Report, MemberFailureReason> {
    let report: serde_json::Value =
        serde_json::from_str(line).map_err(|error| MemberFailureReason::UnreadableReport {
            why: format!("it does not parse as JSON ({error})"),
        })?;
    let reported = report
        .get("formation_id")
        .and_then(serde_json::Value::as_str);
    if reported != Some(expected.as_str()) {
        return Err(MemberFailureReason::WrongFormation {
            reported: reported.map(str::to_string),
            expected: expected.as_str().to_string(),
        });
    }
    let session_id = report
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| MemberFailureReason::UnreadableReport {
            why: "it names no session_id".to_string(),
        })?;
    let url = report
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if url.is_empty() {
        return Err(MemberFailureReason::ReportedNoDoor);
    }
    Ok(Report {
        session_id: session_id.to_string(),
        url: format!("http://{}", url.trim_start_matches("http://")),
        door_token: child_launch::readiness_door_token(&report),
    })
}

// ── The launch ────────────────────────────────────────────────────────────────

/// Start every peer of `roster` at once and wait for each door to answer.
///
/// On success the peers are running and ready, and the entry member is not yet started: the caller
/// announces the formation, then calls [`RunningFormation::start_entry`]. On failure nothing this
/// started is still running.
pub fn launch_formation(
    roster: &AdmittedRoster,
    options: FormationLaunchOptions,
) -> Result<RunningFormation, MemberLaunchFailure> {
    launch_plan(FormationPlan::from_roster(roster), options)
}

pub(crate) fn launch_plan(
    plan: FormationPlan,
    options: FormationLaunchOptions,
) -> Result<RunningFormation, MemberLaunchFailure> {
    install_panic_hook();
    let binary = match child_launch::mur_binary() {
        Ok(binary) => binary,
        Err(error) => {
            return Err(MemberLaunchFailure {
                failed: plan
                    .peers
                    .iter()
                    .chain([&plan.entry])
                    .map(|member| failure(member, not_started(&error.to_string())))
                    .collect(),
                stopped: Vec::new(),
                interrupted_by: None,
            })
        }
    };

    let authority = match FormationAuthority::generate(&options.formation_id) {
        Ok(authority) => authority,
        Err(error) => {
            return Err(MemberLaunchFailure {
                failed: plan
                    .peers
                    .iter()
                    .chain([&plan.entry])
                    .map(|member| failure(member, not_started(&error)))
                    .collect(),
                stopped: Vec::new(),
                interrupted_by: None,
            })
        }
    };
    // One per peer and one for the entry member, all before the first spawn: where the
    // close-on-exec flag is set after `pipe` returns, a member forked in between would inherit
    // another member's write end and keep that member's formation alive.
    #[cfg(unix)]
    let (peer_lifelines, entry_lifeline) = match member_lifelines(plan.peers.len() + 1) {
        Ok(mut lifelines) => {
            let entry = lifelines.pop();
            (lifelines, entry)
        }
        Err(error) => {
            return Err(MemberLaunchFailure {
                failed: plan
                    .peers
                    .iter()
                    .chain([&plan.entry])
                    .map(|member| {
                        failure(
                            member,
                            not_started(&format!("its lifeline could not be created: {error}")),
                        )
                    })
                    .collect(),
                stopped: Vec::new(),
                interrupted_by: None,
            })
        }
    };
    let first_lines: Vec<String> = plan
        .peers
        .iter()
        .map(|member| plan.first_line(&authority, &member.name))
        .collect();

    let abort = AtomicBool::new(false);
    let mut outcomes: Vec<Option<(Option<MemberProcess>, PeerOutcome)>> =
        plan.peers.iter().map(|_| None).collect();
    let mut interrupted_by = caught_signal();
    if interrupted_by.is_some() {
        abort.store(true, Ordering::SeqCst);
    }
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        #[cfg(unix)]
        let mut peer_lifelines = peer_lifelines.into_iter();
        for (index, member) in plan.peers.iter().enumerate() {
            let tx = tx.clone();
            let (binary, options, abort) = (&binary, &options, &abort);
            let first_line = first_lines[index].as_str();
            #[cfg(unix)]
            let lifeline = peer_lifelines
                .next()
                .expect("member_lifelines made one lifeline per peer");
            scope.spawn(move || {
                let started = start_peer(
                    binary,
                    member,
                    first_line,
                    options,
                    abort,
                    #[cfg(unix)]
                    lifeline,
                );
                let _ = tx.send((index, started));
            });
        }
        drop(tx);
        loop {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok((index, outcome)) => {
                    if matches!(outcome.1, PeerOutcome::Failed(_)) {
                        abort.store(true, Ordering::SeqCst);
                    }
                    outcomes[index] = Some(outcome);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if interrupted_by.is_none() {
                if let Some(signal) = caught_signal() {
                    interrupted_by = Some(signal);
                    abort.store(true, Ordering::SeqCst);
                }
            }
        }
    });

    let mut members = MemberSet(Vec::new());
    let mut peers = Vec::new();
    let mut failed = Vec::new();
    for (member, outcome) in plan.peers.iter().zip(outcomes) {
        let Some((process, outcome)) = outcome else {
            continue;
        };
        members.0.extend(process);
        match outcome {
            PeerOutcome::Ready(peer) => peers.push(peer),
            PeerOutcome::Failed(reason) => failed.push(failure(member, reason)),
            PeerOutcome::Abandoned => {}
        }
    }

    if !failed.is_empty() || interrupted_by.is_some() || peers.len() != plan.peers.len() {
        let stopped = members.stop_all();
        return Err(MemberLaunchFailure {
            failed,
            stopped,
            interrupted_by,
        });
    }

    // Every peer's door answers, so every address is known: each peer that calls other peers is
    // told where they are.
    for member in &mut members.0 {
        let addresses = addresses_of(&peers, plan.callees_of(&member.name));
        if !addresses.is_empty() {
            member.send_line(&render_address_line(&addresses));
        }
    }

    Ok(RunningFormation {
        formation_id: options.formation_id.clone(),
        entry: plan.entry.clone(),
        plan,
        peers,
        members,
        entry_index: None,
        #[cfg(unix)]
        entry_lifeline,
        binary,
        options,
        authority,
    })
}

/// The door of each of `callees` among the ready peers, in roster order.
fn addresses_of(ready: &[ReadyPeer], callees: &[String]) -> Vec<FormationPeer> {
    ready
        .iter()
        .filter(|peer| callees.contains(&peer.name))
        .map(|peer| FormationPeer {
            name: peer.name.clone(),
            url: peer.url.clone(),
        })
        .collect()
}

/// `count` new lifelines, or the first error creating one.
#[cfg(unix)]
fn member_lifelines(count: usize) -> std::io::Result<Vec<crate::lifeline::MemberLifeline>> {
    (0..count)
        .map(|_| crate::lifeline::MemberLifeline::new())
        .collect()
}

fn failure(member: &PlannedMember, reason: MemberFailureReason) -> MemberFailure {
    MemberFailure {
        name: member.name.clone(),
        capsule: member.capsule.clone(),
        version: member.version.clone(),
        reason,
    }
}

fn not_started(error: &str) -> MemberFailureReason {
    MemberFailureReason::NotStarted {
        error: error.to_string(),
    }
}

/// A formation whose peers are ready: the one owner of every member it started, and of the
/// formation's signing key.
///
/// Dropping it stops and reaps every member, then drops the key: nothing can mint for this
/// formation again.
pub struct RunningFormation {
    formation_id: FormationId,
    entry: PlannedMember,
    plan: FormationPlan,
    peers: Vec<ReadyPeer>,
    /// Declared before `authority`, so the members are stopped and reaped before the key is
    /// dropped.
    members: MemberSet,
    /// The entry member's position in `members`, once started.
    entry_index: Option<usize>,
    /// The entry member's lifeline, created with the peers' and held here until the entry
    /// member is started with it.
    #[cfg(unix)]
    entry_lifeline: Option<crate::lifeline::MemberLifeline>,
    binary: PathBuf,
    options: FormationLaunchOptions,
    authority: FormationAuthority,
}

impl std::fmt::Debug for RunningFormation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningFormation")
            .field("formation_id", &self.formation_id)
            .field("entry", &self.entry.name)
            .field("peers", &self.peers)
            .finish()
    }
}

impl RunningFormation {
    pub fn formation_id(&self) -> &FormationId {
        &self.formation_id
    }

    /// The entry member's roster name.
    pub fn entry_name(&self) -> &str {
        &self.entry.name
    }

    /// Every peer, ready, in roster order.
    pub fn peers(&self) -> &[ReadyPeer] {
        &self.peers
    }

    /// The members `member` is handed a credential and an address for, in roster order: its
    /// roster callees, less the entry member.
    pub fn callees_of(&self, member: &str) -> &[String] {
        self.plan.callees_of(member)
    }

    /// The entry member's pid, once started.
    pub fn entry_pid(&self) -> Option<u32> {
        self.entry_index.map(|index| self.members.0[index].pid)
    }

    /// Start the entry member, last, with its standard output and error this process's own.
    ///
    /// On failure every peer has been stopped and reaped.
    pub fn start_entry(&mut self) -> Result<u32, MemberLaunchFailure> {
        let args = entry_args(&self.entry, &self.options);
        // Its whole channel is written before it starts: every address it may need is known.
        let channel = Channel::open(&self.plan.first_line(&self.authority, &self.entry.name))
            .and_then(|mut channel| {
                let addresses = addresses_of(&self.peers, self.plan.callees_of(&self.entry.name));
                if !addresses.is_empty() {
                    channel
                        .writer
                        .write_all(format!("{}\n", render_address_line(&addresses)).as_bytes())
                        .map_err(|error| {
                            not_started(&format!(
                                "its formation channel could not be written: {error}"
                            ))
                        })?;
                }
                Ok(channel)
            });
        let channel = match channel {
            Ok(channel) => channel,
            Err(reason) => {
                let stopped = self.members.stop_all();
                return Err(MemberLaunchFailure {
                    failed: vec![failure(&self.entry, reason)],
                    stopped,
                    interrupted_by: None,
                });
            }
        };
        let set = channel.member_env(&self.formation_id);
        let started = child_launch::start_process(&ProcessLaunch {
            binary: &self.binary,
            args: &args,
            cwd: None,
            env: ProcessEnv::Inherited {
                set: &set,
                remove: &[SPAWNER_ENV, FORMATION_PEERS_ENV],
            },
            stdin_line: None,
            stderr_prefix: None,
            inherit_output: true,
            own_process_group: false,
            inherit_fd: Some(channel.fd()),
            #[cfg(unix)]
            lifeline: self.entry_lifeline.as_ref(),
        });
        let Channel { writer, reader } = channel;
        drop(reader);
        match started {
            Ok(started) => {
                #[cfg(unix)]
                let lifeline = self.entry_lifeline.take().map(|mut lifeline| {
                    lifeline.spawned();
                    lifeline
                });
                let process = MemberProcess::new(
                    &self.entry.name,
                    started.child,
                    false,
                    self.options.stop_grace,
                    writer,
                    #[cfg(unix)]
                    lifeline,
                );
                let pid = process.pid;
                self.members.0.push(process);
                self.entry_index = Some(self.members.0.len() - 1);
                Ok(pid)
            }
            Err(error) => {
                let stopped = self.members.stop_all();
                Err(MemberLaunchFailure {
                    failed: vec![failure(
                        &self.entry,
                        not_started(&format!("{}: {error}", self.binary.display())),
                    )],
                    stopped,
                    interrupted_by: None,
                })
            }
        }
    }

    /// Wait for the entry member's process to end, or for the launcher to be signalled, then stop
    /// and reap every member: every lifeline closed, the entry member's included.
    pub fn wait(mut self) -> FormationExit {
        let ending = match self.entry_index {
            None => caught_signal()
                .map(FormationEnding::Signalled)
                .unwrap_or(FormationEnding::EntryExited(ExitStatus::default())),
            Some(index) => loop {
                if let Some(status) = self.members.0[index].exit_status_now() {
                    break FormationEnding::EntryExited(status);
                }
                if let Some(signal) = caught_signal() {
                    break FormationEnding::Signalled(signal);
                }
                std::thread::sleep(POLL_INTERVAL);
            },
        };
        let stopped = self.members.stop_all();
        FormationExit { ending, stopped }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// The production bounds, stated: the readiness deadline a delegated child also gets, and a
    /// grace that outlasts the runtime's own teardown deadline.
    #[test]
    fn the_production_bounds_are_180s_and_a_grace_past_the_runtime_teardown() {
        assert_eq!(MEMBER_READY_TIMEOUT, Duration::from_secs(180));
        assert_eq!(MEMBER_STOP_GRACE, Duration::from_secs(25));
        assert!(MEMBER_STOP_GRACE > crate::runtime::TERMINATE_TEARDOWN_DEADLINE);
    }

    fn member(name: &str) -> PlannedMember {
        PlannedMember {
            name: name.to_string(),
            capsule: name.to_string(),
            version: "0.1.0".to_string(),
            sha256: "ab".repeat(32),
        }
    }

    fn options(deadline: Duration) -> FormationLaunchOptions {
        let mut options =
            FormationLaunchOptions::new(FormationId::mint(), PathBuf::from("/tmp/project"));
        options.ready_timeout = deadline;
        options.stop_grace = Duration::from_secs(2);
        options
    }

    #[test]
    fn member_argv_is_exactly_the_documented_shape() {
        let mut options = options(MEMBER_READY_TIMEOUT);
        let coder = member("coder");
        assert_eq!(
            peer_args(&coder, &options),
            [
                "run",
                "--capsule",
                "coder",
                "--capsule-version",
                "0.1.0",
                "--capsule-sha256",
                &"ab".repeat(32),
                "--workdir",
                "/tmp/project",
                "--json",
                "--bind",
                "127.0.0.1",
                "--ignore-task-file",
            ]
        );
        options.task = Some("probe".to_string());
        options.json = true;
        options.verbose = true;
        options.no_env_file = true;
        options.containment = Some("scoped".to_string());
        let entry = entry_args(&member("planner"), &options);
        assert_eq!(
            &entry[9..],
            [
                "--lifecycle-task-acceptance",
                "single",
                "--lifecycle-after-task",
                "exit",
                "--task",
                "probe",
                "--json",
                "--verbose",
                "--no-env-file",
                "--containment",
                "scoped",
            ]
        );
        assert!(
            !entry
                .iter()
                .any(|arg| arg == "--bind" || arg == "--ignore-task-file"),
            "{entry:?}"
        );
        let peer = peer_args(&coder, &options);
        assert!(peer.ends_with(&[
            "--no-env-file".to_string(),
            "--containment".to_string(),
            "scoped".to_string()
        ]));
        assert!(!peer.iter().any(|arg| arg == "--task" || arg == "--verbose"));
    }

    /// Serializes the tests that point `MURMUR_MUR_BINARY` at a stand-in; the variable is
    /// process-wide.
    fn binary_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A stand-in `mur` that prints `line` (when given), records any `SIGTERM` it is sent in
    /// `dir/sigterm`, and runs until its lifeline reads EOF, whatever it is asked to run.
    fn stand_in(dir: &Path, line: Option<&str>) -> PathBuf {
        let script = dir.join("mur");
        let print = match line {
            Some(line) => {
                std::fs::write(dir.join("line"), format!("{line}\n")).unwrap();
                format!("cat '{}'\n", dir.join("line").display())
            }
            None => String::new(),
        };
        // Through `/dev/fd`: a POSIX shell's `<&` takes a single-digit descriptor only, and a
        // lifeline is numbered wherever `pipe` put it.
        let body = format!(
            "#!/bin/sh\ntrap 'echo TERM >> \"{dir}/sigterm\"' TERM\n{print}\
             cat \"/dev/fd/$MURMUR_FORMATION_LIFELINE\" >/dev/null\n",
            dir = dir.display()
        );
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn plan_of(peer: &str) -> FormationPlan {
        FormationPlan {
            peers: vec![member(peer)],
            entry: member("planner"),
            callees: vec![("planner".to_string(), vec![peer.to_string()])],
        }
    }

    /// Launch one peer from a stand-in printing `line`, returning the refusal and the stand-in's
    /// pid as the refusal named it.
    fn refused(line: Option<&str>, deadline: Duration) -> (MemberLaunchFailure, FormationId) {
        let _guard = binary_guard();
        let dir = tempfile::tempdir().unwrap();
        let options = options(deadline);
        let id = options.formation_id.clone();
        let line = line.map(|line| line.replace("{formation}", id.as_str()));
        std::env::set_var(
            child_launch::MUR_BINARY_ENV,
            stand_in(dir.path(), line.as_deref()),
        );
        let result = launch_plan(plan_of("coder"), options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        match result {
            Ok(_) => panic!("the launch was expected to be refused"),
            Err(failure) => (failure, id),
        }
    }

    fn assert_reaped(failure: &MemberLaunchFailure) {
        assert_eq!(failure.stopped.len(), 1, "{failure:?}");
        let pid = failure.stopped[0].pid;
        assert_eq!(failure.stopped[0].name, "coder");
        assert!(
            !crate::running::pid_is_alive(pid),
            "the stand-in {pid} is still running"
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists() || cfg!(not(target_os = "linux")),
            "the stand-in {pid} was not reaped"
        );
    }

    fn only_reason(failure: &MemberLaunchFailure) -> &MemberFailureReason {
        assert_eq!(failure.failed.len(), 1, "{failure:?}");
        assert_eq!(failure.failed[0].name, "coder");
        &failure.failed[0].reason
    }

    /// A port with nothing listening on it.
    fn silent_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_door_nothing_answers_on_is_refused_at_the_deadline() {
        let port = silent_port();
        let started = Instant::now();
        let (failure, _) = refused(
            Some(&format!(
                r#"{{"formation_id":"{{formation}}","session_id":"ses_standin","url":"localhost:{port}"}}"#
            )),
            Duration::from_millis(800),
        );
        assert!(started.elapsed() >= Duration::from_millis(800));
        match only_reason(&failure) {
            MemberFailureReason::DoorDidNotAnswer {
                url, session_id, ..
            } => {
                assert_eq!(url, &format!("http://localhost:{port}"));
                assert_eq!(session_id, "ses_standin");
            }
            other => panic!("{other:?}"),
        }
        let message = failure.to_string();
        assert!(
            message.contains("did not answer as session ses_standin"),
            "{message}"
        );
        assert!(message.contains("'coder' (coder@0.1.0)"), "{message}");
        assert_reaped(&failure);
    }

    /// A one-route door: `GET /.well-known/agent-card.json` answered with a card naming
    /// `session_id`, from `answer_after` on; refused connections before then are emulated by not
    /// accepting.
    fn door(session_id: &'static str, answer_after: Duration) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let opened = Instant::now();
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let body = if opened.elapsed() >= answer_after {
                    serde_json::json!({
                        "name": "coder",
                        "capabilities": {"extensions": [{
                            "uri": crate::identity::CAPSULE_EXTENSION_URI,
                            "params": {"sessionId": session_id},
                        }]},
                    })
                    .to_string()
                } else {
                    "{}".to_string()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    #[test]
    fn a_door_naming_another_session_is_not_ready() {
        let port = door("ses_somebody_else", Duration::ZERO);
        let (failure, _) = refused(
            Some(&format!(
                r#"{{"formation_id":"{{formation}}","session_id":"ses_standin","url":"localhost:{port}"}}"#
            )),
            Duration::from_millis(800),
        );
        match only_reason(&failure) {
            MemberFailureReason::DoorDidNotAnswer { last_error, .. } => {
                assert!(last_error.contains("ses_somebody_else"), "{last_error}")
            }
            other => panic!("{other:?}"),
        }
        assert_reaped(&failure);
    }

    #[test]
    fn an_empty_url_is_refused_as_no_door() {
        let (failure, _) = refused(
            Some(r#"{"formation_id":"{formation}","session_id":"ses_standin","url":""}"#),
            Duration::from_secs(10),
        );
        assert_eq!(only_reason(&failure), &MemberFailureReason::ReportedNoDoor);
        assert!(failure.to_string().contains("it reported no door"));
        assert_reaped(&failure);
    }

    #[test]
    fn a_wrong_formation_or_none_is_refused_naming_what_was_reported() {
        let other = FormationId::mint();
        let (failure, id) = refused(
            Some(&format!(
                r#"{{"formation_id":"{}","session_id":"ses_standin","url":"localhost:1"}}"#,
                other.as_str()
            )),
            Duration::from_secs(10),
        );
        assert_eq!(
            only_reason(&failure),
            &MemberFailureReason::WrongFormation {
                reported: Some(other.as_str().to_string()),
                expected: id.as_str().to_string(),
            }
        );
        assert!(failure
            .to_string()
            .contains(&format!("it reported formation {other} instead of {id}")));
        assert_reaped(&failure);

        let (failure, id) = refused(
            Some(r#"{"session_id":"ses_standin","url":"localhost:1"}"#),
            Duration::from_secs(10),
        );
        assert!(failure
            .to_string()
            .contains(&format!("it reported no formation instead of {id}")));
        assert_reaped(&failure);
    }

    #[test]
    fn no_line_at_all_is_refused_at_the_deadline() {
        let started = Instant::now();
        let (failure, _) = refused(None, Duration::from_millis(600));
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert_eq!(
            only_reason(&failure),
            &MemberFailureReason::DidNotReport {
                deadline: Duration::from_millis(600)
            }
        );
        assert_reaped(&failure);
    }

    #[test]
    fn a_door_that_starts_answering_before_the_deadline_is_ready() {
        let _guard = binary_guard();
        let port = door("ses_standin", Duration::from_millis(400));
        let dir = tempfile::tempdir().unwrap();
        let options = options(Duration::from_secs(20));
        let line = format!(
            r#"{{"formation_id":"{}","session_id":"ses_standin","url":"localhost:{port}"}}"#,
            options.formation_id
        );
        std::env::set_var(
            child_launch::MUR_BINARY_ENV,
            stand_in(dir.path(), Some(&line)),
        );
        let result = launch_plan(plan_of("coder"), options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        let formation = result.expect("the door answered before the deadline");
        assert_eq!(formation.peers().len(), 1);
        let peer = &formation.peers()[0];
        assert_eq!(peer.session_id, "ses_standin");
        assert_eq!(peer.url, format!("http://localhost:{port}"));
        assert_eq!(formation.callees_of("planner"), ["coder"]);
        assert!(formation.callees_of("coder").is_empty());
        let pid = peer.pid;
        assert!(crate::running::pid_is_alive(pid));
        let dropped = Instant::now();
        drop(formation);
        assert!(
            !crate::running::pid_is_alive(pid),
            "dropping the owner stops the peer"
        );
        assert!(
            dropped.elapsed() < Duration::from_secs(2),
            "the peer ended on its lifeline's EOF, inside the grace: {:?}",
            dropped.elapsed()
        );
        assert!(
            !dir.path().join("sigterm").exists(),
            "the launcher sent the peer SIGTERM"
        );
    }

    /// A member that does not end on EOF is killed with its process group once the grace has run.
    #[test]
    fn a_member_deaf_to_its_lifeline_is_killed_after_the_grace() {
        let _guard = binary_guard();
        let port = door("ses_standin", Duration::ZERO);
        let dir = tempfile::tempdir().unwrap();
        let mut options = options(Duration::from_secs(20));
        options.stop_grace = Duration::from_millis(600);
        let line = format!(
            r#"{{"formation_id":"{}","session_id":"ses_standin","url":"localhost:{port}"}}"#,
            options.formation_id
        );
        let script = dir.path().join("mur");
        std::fs::write(dir.path().join("line"), format!("{line}\n")).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat '{}'\nexec sleep 600\n",
                dir.path().join("line").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(child_launch::MUR_BINARY_ENV, &script);
        let result = launch_plan(plan_of("coder"), options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        let formation = result.expect("the door answers");
        let pid = formation.peers()[0].pid;
        let dropped = Instant::now();
        drop(formation);
        assert!(dropped.elapsed() >= Duration::from_millis(600));
        assert!(!crate::running::pid_is_alive(pid));
    }

    /// Every member reads exactly its own bundle on the descriptor `MURMUR_FORMATION_CHANNEL`
    /// names: a token per callee and nothing about anyone else, then — once every peer is ready —
    /// its callees' doors. The entry member gets both lines before it starts, and an edge into it
    /// is served to nobody. No token is in any member's argv or environment.
    #[test]
    fn each_member_reads_only_its_own_credentials_and_callees_on_its_channel() {
        use crate::formation_credentials::{parse_address_line, FormationBundle};
        let _guard = binary_guard();
        let dir = tempfile::tempdir().unwrap();
        let coder_port = door("ses_coder", Duration::ZERO);
        let reviewer_port = door("ses_reviewer", Duration::ZERO);
        let options = options(Duration::from_secs(20));
        let id = options.formation_id.clone();
        let line = |session: &str, port: u16| {
            format!(
                r#"{{"formation_id":"{id}","session_id":"{session}","url":"localhost:{port}"}}"#
            )
        };
        let out = dir.path().display().to_string();
        let script = dir.path().join("mur");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 case \" $* \" in\n\
                 *' --capsule coder '*) name=coder; line='{coder}';;\n\
                 *' --capsule reviewer '*) name=reviewer; line='{reviewer}';;\n\
                 *) name=planner; line='';;\n\
                 esac\n\
                 env > '{out}/'$name.env\n\
                 printf '%s\\n' \"$*\" > '{out}/'$name.argv\n\
                 cat /dev/fd/$MURMUR_FORMATION_CHANNEL >> '{out}/'$name.channel &\n\
                 [ -n \"$line\" ] && printf '%s\\n' \"$line\"\n\
                 exec sleep 600\n",
                coder = line("ses_coder", coder_port),
                reviewer = line("ses_reviewer", reviewer_port),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var(child_launch::MUR_BINARY_ENV, &script);
        let plan = FormationPlan {
            peers: vec![member("coder"), member("reviewer")],
            entry: member("planner"),
            // `reviewer → planner` is an edge into the entry member: `from_roster` leaves it out.
            callees: vec![
                ("planner".to_string(), vec!["coder".to_string()]),
                ("coder".to_string(), vec!["reviewer".to_string()]),
            ],
        };
        let result = launch_plan(plan, options);
        let mut formation = result.expect("both stand-ins came up");
        formation.start_entry().expect("the entry stand-in starts");
        std::env::remove_var(child_launch::MUR_BINARY_ENV);

        let read_lines = |name: &str, count: usize| -> Vec<String> {
            let path = dir.path().join(format!("{name}.channel"));
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let lines: Vec<String> = std::fs::read_to_string(&path)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect();
                if lines.len() >= count || Instant::now() >= deadline {
                    return lines;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        };
        let coder = read_lines("coder", 2);
        let planner = read_lines("planner", 2);
        let reviewer = read_lines("reviewer", 1);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(read_lines("reviewer", 1).len(), 1, "reviewer calls nobody");

        let bundle = |line: &str| FormationBundle::parse_line(line).expect("a member's bundle");
        let names = |bundle: &FormationBundle| -> Vec<String> {
            bundle.calls.iter().map(|(name, _)| name.clone()).collect()
        };
        let (coder_bundle, planner_bundle, reviewer_bundle) =
            (bundle(&coder[0]), bundle(&planner[0]), bundle(&reviewer[0]));
        assert_eq!(coder_bundle.member, "coder");
        assert_eq!(names(&coder_bundle), ["reviewer"]);
        assert_eq!(names(&planner_bundle), ["coder"]);
        assert!(names(&reviewer_bundle).is_empty());
        for bundle in [&coder_bundle, &planner_bundle, &reviewer_bundle] {
            assert_eq!(bundle.formation_id, id);
            assert_eq!(bundle.verify_key, coder_bundle.verify_key);
        }
        assert!(!planner.join("\n").contains("reviewer"), "{planner:?}");
        let addresses = |line: &str| -> Vec<(String, String)> {
            parse_address_line(line)
                .unwrap()
                .into_iter()
                .map(|peer| (peer.name, peer.url))
                .collect()
        };
        assert_eq!(
            addresses(&coder[1]),
            [(
                "reviewer".to_string(),
                format!("http://localhost:{reviewer_port}")
            )]
        );
        assert_eq!(
            addresses(&planner[1]),
            [(
                "coder".to_string(),
                format!("http://localhost:{coder_port}")
            )]
        );

        for name in ["coder", "reviewer", "planner"] {
            let env = std::fs::read_to_string(dir.path().join(format!("{name}.env"))).unwrap();
            let argv = std::fs::read_to_string(dir.path().join(format!("{name}.argv"))).unwrap();
            assert!(!env.contains("mft1.") && !argv.contains("mft1."), "{name}");
            assert!(!env.contains(FORMATION_PEERS_ENV), "{name}: {env}");
            assert!(
                env.contains(&format!("{FORMATION_CHANNEL_ENV}=")),
                "{name}: {env}"
            );
        }
        let pids: Vec<u32> = formation.peers().iter().map(|peer| peer.pid).collect();
        drop(formation);
        for pid in pids {
            assert!(!crate::running::pid_is_alive(pid), "{pid}");
        }
    }

    #[test]
    fn a_registered_member_is_killed_by_the_panic_hooks_sweep() {
        use std::os::unix::process::CommandExt;
        // The sweep kills every member this process registered, so no other launch may be live.
        let _guard = binary_guard();
        let mut child = std::process::Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .unwrap();
        let slot = RegistrySlot::register(child.id(), true);
        assert!(slot.0.is_some());
        kill_registered_members();
        let status = child.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        drop(slot);
        assert!(MEMBER_REGISTRY
            .iter()
            .all(|slot| slot.load(Ordering::SeqCst) != i64::from(child.id())));
    }

    #[test]
    fn a_launch_failure_names_each_member_and_every_stopped_one() {
        let failure = MemberLaunchFailure {
            failed: vec![failure(
                &member("broken"),
                MemberFailureReason::ExitedBeforeReporting {
                    status: "status 1".to_string(),
                    stderr_tail: vec!["error[E-RUN-008]: missing".to_string()],
                },
            )],
            stopped: vec![
                StoppedMember {
                    name: "coder".to_string(),
                    pid: 7,
                },
                StoppedMember {
                    name: "reviewer".to_string(),
                    pid: 8,
                },
            ],
            interrupted_by: None,
        };
        assert_eq!(
            failure.to_string(),
            "the formation could not be launched\n  'broken' (broken@0.1.0): its process exited \
             with status 1 before reporting; its last stderr lines were:\n      \
             error[E-RUN-008]: missing\n  stopped: coder (pid 7), reviewer (pid 8)"
        );
    }

    #[test]
    fn ready_peer_debug_prints_no_door_token() {
        let peer = ReadyPeer {
            name: "coder".to_string(),
            capsule: "coder".to_string(),
            version: "1.2.0".to_string(),
            pid: 1,
            session_id: "ses_x".to_string(),
            url: "http://localhost:1".to_string(),
            door_token: Some(DoorToken::new("mdt1.secretpayload.mac".to_string())),
        };
        let debug = format!("{peer:?}");
        assert!(!debug.contains("secretpayload"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
