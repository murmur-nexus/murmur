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
//! variable. Callees never include the entry member: it runs the formation's own task from launch
//! until the formation ends and serves no peer, and admission never admits an edge into it. The
//! key and the write ends are dropped once every member is reaped.
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
use crate::lifeline::SPAWNER_LIFELINE_ENV;
use crate::roster::AdmittedRoster;
use crate::running::ProcessIdentity;
use serde::{Deserialize, Serialize};

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

/// The hidden `mur run` flag naming the directory a peer resolves its artifacts, its lock and its
/// workspace from, apart from its `--workdir`.
pub const STORE_ROOT_FLAG: &str = "--store-root";

/// The directory under the murmur home that holds every formation's peer directories.
pub const FORMATIONS_DIR: &str = "formations";

/// The ownership marker in `<murmur home>/formations/<frm_id>/`, a [`FormationMarker`]. Member
/// names start with a letter and hold no `.`, so it never collides with a member's directory.
pub const FORMATION_MARKER_FILE: &str = "formation.json";

/// What a formation launch is given besides its roster.
#[derive(Debug, Clone)]
pub struct FormationLaunchOptions {
    /// The id every member is launched with, minted once for this launch.
    pub formation_id: FormationId,
    /// The roster's project directory: the entry member's `--workdir`, and every peer's
    /// `--store-root`, so every member resolves from the stores admission read.
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
    /// Where peers' directories are made in place of `<murmur home>/formations`. `None` in
    /// production; set by tests that must not write under the developer's home.
    pub(crate) formations_dir: Option<PathBuf>,
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
            formations_dir: None,
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
    /// Every member's callees in roster order. Admission never admits an edge into the entry
    /// member, so it is never among them. A member with none is absent.
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

    /// `member`'s channel first line, signed by `authority` and naming `launcher` as the process
    /// that started it.
    fn first_line(
        &self,
        authority: &FormationAuthority,
        launcher: Option<&ProcessIdentity>,
        member: &str,
    ) -> String {
        let callees: Vec<&str> = self.callees_of(member).iter().map(String::as_str).collect();
        let mut bundle = authority.member_bundle(member, &callees);
        bundle.launcher = launcher.cloned();
        bundle.render_line()
    }
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
    /// The peer's accessible directory, `<murmur home>/formations/<frm_id>/<member>`: its
    /// `--workdir`, where its `task.md` is written and its sessions nest.
    pub workdir: PathBuf,
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
            .field("workdir", &self.workdir)
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

/// The arguments every member's `mur run` starts with, running in `workdir`.
fn member_args(member: &PlannedMember, workdir: &Path) -> Vec<String> {
    vec![
        "run".to_string(),
        "--capsule".to_string(),
        member.capsule.clone(),
        "--capsule-version".to_string(),
        member.version.clone(),
        CAPSULE_SHA256_FLAG.to_string(),
        member.sha256.clone(),
        "--workdir".to_string(),
        workdir.display().to_string(),
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

/// A peer's arguments after the binary: it runs in `workdir`, a directory of its own, and resolves
/// from the project's stores. Its readiness line carries its tokens, which this launcher reads on
/// the peer's stdout pipe for the door probe.
pub(crate) fn peer_args(
    member: &PlannedMember,
    options: &FormationLaunchOptions,
    workdir: &Path,
) -> Vec<String> {
    let mut args = member_args(member, workdir);
    args.extend([
        STORE_ROOT_FLAG.to_string(),
        options.project_dir.display().to_string(),
        "--json".to_string(),
        child_launch::READINESS_TOKENS_FLAG.to_string(),
        "--bind".to_string(),
        PEER_BIND_ADDR.to_string(),
    ]);
    pass_through(&mut args, options);
    args
}

/// The entry member's arguments after the binary. It runs in the project directory, where its
/// `--task` is written. Never [`child_launch::READINESS_TOKENS_FLAG`]: the entry's output is the
/// operator's.
pub(crate) fn entry_args(member: &PlannedMember, options: &FormationLaunchOptions) -> Vec<String> {
    let mut args = member_args(member, &options.project_dir);
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

/// `<murmur home>/formations`, resolved without creating anything.
fn formations_dir() -> Result<PathBuf, String> {
    Ok(crate::state_store::murmur_home_dir()?.join(FORMATIONS_DIR))
}

/// Every session root a formation's peers record under: `<dir>/.murmur` for each directory under
/// `<murmur home>/formations/<frm_id>/`, sorted.
///
/// Reads the directory and creates nothing. A formation with no directory, or a `HOME` that does
/// not resolve, has no roots. A formation directory that exists and cannot be listed is returned
/// as a root itself, so the caller's read of it fails and is reported as an unreadable root.
pub fn formation_member_roots(formation_id: &FormationId) -> Vec<PathBuf> {
    match formations_dir() {
        Ok(dir) => formation_member_roots_in(&dir, formation_id),
        Err(_) => Vec::new(),
    }
}

/// [`formation_member_roots`] under `formations_dir` in place of `<murmur home>/formations`.
pub fn formation_member_roots_in(
    formations_dir: &Path,
    formation_id: &FormationId,
) -> Vec<PathBuf> {
    let dir = formations_dir.join(formation_id.as_str());
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => return vec![dir],
    };
    let mut roots: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path().join(".murmur"))
        .filter(|root| root.exists())
        .collect();
    roots.sort();
    roots
}

/// Who launched a formation, from which project: the contents of [`FORMATION_MARKER_FILE`].
///
/// The launcher writes it once, before the first peer's directory is made, and nothing rewrites
/// it. Retention reads it to decide whether a formation directory is this project's to remove and
/// whether the launcher that made it is still running; a directory without one is never removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormationMarker {
    /// The roster's project directory, canonicalized when the launcher could, as given otherwise.
    pub project_dir: PathBuf,
    /// The launcher's pid.
    pub launcher_pid: u32,
    /// The launcher's start token, read as [`crate::running::process_start_token`] reads it, or
    /// `""` when it could not be read.
    pub launcher_start: String,
}

impl FormationMarker {
    /// The marker for a formation this process launches from `project_dir`.
    pub fn for_this_launcher(project_dir: &Path) -> Self {
        let pid = std::process::id();
        Self {
            project_dir: canonical_project_dir(project_dir),
            launcher_pid: pid,
            launcher_start: crate::running::process_start_token(pid).unwrap_or_default(),
        }
    }

    /// The marker in `formation_dir`, or `None` when it is missing or does not parse.
    pub fn read(formation_dir: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(formation_dir.join(FORMATION_MARKER_FILE)).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

/// `project_dir` canonicalized, or as given when it cannot be: the one form a marker records and
/// the one ownership is compared in.
pub(crate) fn canonical_project_dir(project_dir: &Path) -> PathBuf {
    std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf())
}

/// Write this launch's [`FormationMarker`] into `<formations dir>/<frm_id>/`, holding both
/// directories owner-only first. The marker itself is written at `0600`.
fn write_formation_marker(options: &FormationLaunchOptions) -> Result<(), String> {
    let formations_dir = launch_formations_dir(options)?;
    let formation_dir = formations_dir.join(options.formation_id.as_str());
    for dir in [formations_dir.as_path(), formation_dir.as_path()] {
        crate::state_store::ensure_private_dir(dir, crate::murmur_home::MURMUR_HOME_DIR_MODE)
            .map_err(|reason| format!("{}: {reason}", dir.display()))?;
    }
    let marker = FormationMarker::for_this_launcher(&options.project_dir);
    let json = serde_json::to_string(&marker).map_err(|error| error.to_string())?;
    crate::murmur_home::write_private_file(
        &formation_dir.join(FORMATION_MARKER_FILE),
        format!("{json}\n").as_bytes(),
    )
}

/// Remove the ended formation directories of `project_dir` that the entry member's `policy` does
/// not keep, and return what went.
///
/// Called by the launcher once every member of `current` has been stopped and reaped. `current`
/// and every formation minted after it are never removed; see [`crate::retention::prune_formations`]
/// for what else never is. A `HOME` that does not resolve removes nothing.
pub fn prune_ended_formations(
    current: &FormationId,
    project_dir: &Path,
    policy: &murmur_artifact::TraceRetainConfig,
) -> Vec<crate::retention::PrunedFormation> {
    let (Ok(formations_dir), Ok(running_dir)) =
        (formations_dir(), crate::running::running_dir_location())
    else {
        return Vec::new();
    };
    prune_ended_formations_in(&formations_dir, &running_dir, current, project_dir, policy)
}

/// [`prune_ended_formations`] over `formations_dir` and `running_dir` in place of the murmur
/// home's.
pub fn prune_ended_formations_in(
    formations_dir: &Path,
    running_dir: &Path,
    current: &FormationId,
    project_dir: &Path,
    policy: &murmur_artifact::TraceRetainConfig,
) -> Vec<crate::retention::PrunedFormation> {
    // One scan for the whole pass, and only a scan: nothing here unlinks a record.
    let records = crate::running::scan(running_dir)
        .ok()
        .map(|scan| scan.records);
    crate::retention::prune_formations(
        formations_dir,
        current,
        project_dir,
        policy,
        crate::retention::now_ms(),
        |id, marker| formation_is_live(id, marker, records.as_deref()),
    )
}

/// Whether any part of formation `id` may still be running: its launcher is not gone, or a running
/// record names `id` and its process is not gone.
///
/// `records` is `None` when the running directory could not be listed, and then every formation
/// is live. [`crate::running::ProcessState::Unverified`] is live: a reading that failed says
/// nothing about the process.
fn formation_is_live(
    id: &FormationId,
    marker: &FormationMarker,
    records: Option<&[crate::running::RunningRecord]>,
) -> bool {
    let not_gone = |state| !matches!(state, crate::running::ProcessState::Gone(_));
    let Some(records) = records else {
        return true;
    };
    not_gone(crate::running::process_state_of(
        marker.launcher_pid,
        &marker.launcher_start,
    )) || records.iter().any(|record| {
        record.formation_id.as_ref() == Some(id) && not_gone(crate::running::process_state(record))
    })
}

/// Make `member`'s directory under `formations_dir`, owner-only, and return it.
///
/// `formations_dir` and the formation's directory are held at `0700` whether or not this call made
/// them. The member's own directory is made with a single non-recursive create at `0700`, so a
/// path that already exists — a directory, a file or a symlink — is refused rather than reused:
/// nothing another process placed there becomes a member's preopen.
fn make_member_workdir(
    formations_dir: &Path,
    formation_id: &FormationId,
    member: &str,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;

    let formation_dir = formations_dir.join(formation_id.as_str());
    let workdir = formation_dir.join(member);
    let failed = |path: &Path, reason: String| {
        format!(
            "its directory {} could not be made: {}: {reason}",
            workdir.display(),
            path.display()
        )
    };
    for dir in [formations_dir, formation_dir.as_path()] {
        crate::state_store::ensure_private_dir(dir, crate::murmur_home::MURMUR_HOME_DIR_MODE)
            .map_err(|reason| failed(dir, reason))?;
    }
    std::fs::DirBuilder::new()
        .mode(crate::murmur_home::MURMUR_HOME_DIR_MODE)
        .create(&workdir)
        .map_err(|error| failed(&workdir, error.to_string()))?;
    // The umask may have narrowed the create's mode further; it can never have widened it.
    Ok(workdir)
}

/// The directory peers' directories are made under for this launch: the override, or
/// `<murmur home>/formations` with the home held owner-only first.
fn launch_formations_dir(options: &FormationLaunchOptions) -> Result<PathBuf, String> {
    match &options.formations_dir {
        Some(dir) => Ok(dir.clone()),
        None => {
            crate::murmur_home::ensure_murmur_home()?;
            formations_dir()
        }
    }
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

    /// The environment every member is started with: its formation id, its channel, and
    /// [`crate::host_warnings::HOST_WARNINGS_REPORTED_ENV`], because the launcher prints the
    /// host-level warnings once before any member starts.
    fn member_env(&self, formation_id: &FormationId) -> Vec<(String, String)> {
        let (id_name, id_value) = formation_id.env_pair();
        vec![
            (id_name.to_string(), id_value),
            (FORMATION_CHANNEL_ENV.to_string(), self.fd().to_string()),
            crate::host_warnings::reported_env_pair(),
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
    let workdir = match launch_formations_dir(options)
        .and_then(|dir| make_member_workdir(&dir, &options.formation_id, &member.name))
    {
        Ok(workdir) => workdir,
        Err(error) => return (None, PeerOutcome::Failed(not_started(&error))),
    };
    let args = peer_args(member, options, &workdir);
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
            remove: &[FORMATION_PEERS_ENV, SPAWNER_ENV, SPAWNER_LIFELINE_ENV],
        },
        stdin_line: None,
        stderr_prefix: Some(&prefix),
        inherit_output: false,
        own_process_group: true,
        inherit_fd: Some(channel.fd()),
        #[cfg(unix)]
        lifeline: Some(crate::child_launch::Lifeline::Member(&lifeline)),
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
        match door_answers_as(
            crate::running::probe_door_session_id(&report.url, report.door_token.as_ref()),
            &report.session_id,
        ) {
            Ok(()) => {
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
                        workdir,
                        door_token: report.door_token,
                    }),
                );
            }
            Err(why) => last_error = why,
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

/// Whether a door probe's answer counts as the reported session being ready: `Ok` only when the
/// card names `reported`. A card naming another session, or a failed probe, is the `Err` that
/// becomes the refusal's `last_error` if the deadline passes.
fn door_answers_as(probed: Result<String, String>, reported: &str) -> Result<(), String> {
    match probed {
        Ok(session_id) if session_id == reported => Ok(()),
        Ok(other) => Err(format!("the door answers as session {other}")),
        Err(error) => Err(error),
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
    // Read once, so every member, the entry member included, records the same launcher. A
    // launcher that cannot read its own start time names none, and `mur stop` then cannot reach
    // it through its members.
    let launcher = ProcessIdentity::of_this_process();
    let first_lines: Vec<String> = plan
        .peers
        .iter()
        .map(|member| plan.first_line(&authority, launcher.as_ref(), &member.name))
        .collect();
    // Before any peer's directory exists, so no formation directory a launch made is without one.
    // A launch with no peers makes no formation directory, and so no marker.
    if !plan.peers.is_empty() {
        if let Err(reason) = write_formation_marker(&options) {
            crate::runtime_err!(
                "[mur run] warning: formation {} has no ownership marker ({reason}); its \
                 directory will never be removed automatically",
                options.formation_id
            );
        }
    }

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
        launcher,
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
    /// This process, as every member's first line names it.
    launcher: Option<ProcessIdentity>,
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
    /// roster callees.
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
        let channel = Channel::open(&self.plan.first_line(
            &self.authority,
            self.launcher.as_ref(),
            &self.entry.name,
        ))
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
                remove: &[SPAWNER_ENV, FORMATION_PEERS_ENV, SPAWNER_LIFELINE_ENV],
            },
            stdin_line: None,
            stderr_prefix: None,
            inherit_output: true,
            own_process_group: false,
            inherit_fd: Some(channel.fd()),
            #[cfg(unix)]
            lifeline: self
                .entry_lifeline
                .as_ref()
                .map(crate::child_launch::Lifeline::Member),
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// How long a launch that is expected to succeed may take before the test gives up on it: a
    /// guard against a launch that never completes on a loaded host, not a claim about how fast
    /// one does.
    const LIVENESS_BOUND: Duration = Duration::from_secs(120);

    /// The host's 1-, 5- and 15-minute load averages, for failure messages that may be load.
    fn load_average() -> String {
        std::fs::read_to_string("/proc/loadavg")
            .unwrap_or_default()
            .split(' ')
            .take(3)
            .collect::<Vec<_>>()
            .join(" ")
    }

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

    /// Every member, peer or entry, is started told its launcher already printed the host-level
    /// warnings.
    #[test]
    fn every_member_is_told_host_warnings_were_reported() {
        let channel = Channel::open("first").expect("a channel opens");
        let env = channel.member_env(&FormationId::mint());
        let values: Vec<&str> = env
            .iter()
            .filter(|(key, _)| key == crate::host_warnings::HOST_WARNINGS_REPORTED_ENV)
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(values, vec!["1"], "{env:?}");
    }

    fn options(deadline: Duration) -> FormationLaunchOptions {
        let mut options =
            FormationLaunchOptions::new(FormationId::mint(), PathBuf::from("/tmp/project"));
        options.ready_timeout = deadline;
        options.stop_grace = Duration::from_secs(2);
        // Peers' directories go under a scratch directory, never the developer's home.
        options.formations_dir = Some(tempfile::tempdir().unwrap().keep());
        options
    }

    #[test]
    fn member_argv_is_exactly_the_documented_shape() {
        let mut options = options(MEMBER_READY_TIMEOUT);
        let coder = member("coder");
        let workdir = PathBuf::from("/home/u/.murmur/formations/frm_x/coder");
        assert_eq!(
            peer_args(&coder, &options, &workdir),
            [
                "run",
                "--capsule",
                "coder",
                "--capsule-version",
                "0.1.0",
                "--capsule-sha256",
                &"ab".repeat(32),
                "--workdir",
                "/home/u/.murmur/formations/frm_x/coder",
                "--store-root",
                "/tmp/project",
                "--json",
                "--readiness-tokens",
                "--bind",
                "127.0.0.1",
            ]
        );
        options.task = Some("probe".to_string());
        options.json = true;
        options.verbose = true;
        options.no_env_file = true;
        options.containment = Some("scoped".to_string());
        let entry = entry_args(&member("planner"), &options);
        assert_eq!(&entry[7..9], ["--workdir", "/tmp/project"]);
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
            !entry.iter().any(|arg| arg == "--bind"
                || arg == STORE_ROOT_FLAG
                || arg == child_launch::READINESS_TOKENS_FLAG),
            "{entry:?}"
        );
        let peer = peer_args(&coder, &options, &workdir);
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

    /// A port with nothing listening on it: claimed, and never bound.
    fn silent_port() -> u16 {
        crate::pinned_port::claim()
    }

    #[test]
    fn a_door_nothing_answers_on_is_refused_at_the_deadline() {
        let deadline = Duration::from_secs(10);
        let port = silent_port();
        let started = Instant::now();
        let (failure, _) = refused(
            Some(&format!(
                r#"{{"formation_id":"{{formation}}","session_id":"ses_standin","url":"localhost:{port}"}}"#
            )),
            deadline,
        );
        assert!(started.elapsed() >= deadline);
        match only_reason(&failure) {
            MemberFailureReason::DoorDidNotAnswer {
                url, session_id, ..
            } => {
                assert_eq!(url, &format!("http://localhost:{port}"));
                assert_eq!(session_id, "ses_standin");
            }
            // One deadline covers the readiness line and the door, so a stand-in that is not
            // scheduled in time is refused before its door is ever probed.
            MemberFailureReason::DidNotReport { .. } => panic!(
                "the stand-in's readiness line did not arrive within {deadline:?} (load average {}); \
                 host load is the likely cause, since the stand-in prints it at once: {failure:?}",
                load_average()
            ),
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

    /// A one-route door on `127.0.0.1`, served forever: each connection's request is read, then
    /// `GET /.well-known/agent-card.json` is answered with a card naming the session `answer`
    /// returns for that request, or with `{}` when it returns `None`.
    fn card_door(mut answer: impl FnMut() -> Option<&'static str> + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let body = match answer() {
                    Some(session_id) => serde_json::json!({
                        "name": "coder",
                        "capabilities": {"extensions": [{
                            "uri": crate::identity::CAPSULE_EXTENSION_URI,
                            "params": {"sessionId": session_id},
                        }]},
                    })
                    .to_string(),
                    None => "{}".to_string(),
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

    /// A door answering with a card naming `session_id` from `answer_after` on, and with a card
    /// naming no session before then.
    fn door(session_id: &'static str, answer_after: Duration) -> u16 {
        let opened = Instant::now();
        card_door(move || (opened.elapsed() >= answer_after).then_some(session_id))
    }

    /// How many card requests [`door_naming_another_session_first`] answers as another session.
    const WRONG_ANSWERS: usize = 3;

    /// A door that answers as `ses_somebody_else` to its first [`WRONG_ANSWERS`] card requests and
    /// as `ses_standin` to every one after, with the count of requests it has read. The count is
    /// raised before the answer is written, so it covers every answer a client has seen.
    fn door_naming_another_session_first() -> (u16, Arc<AtomicUsize>) {
        let served = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&served);
        let port = card_door(move || {
            if counted.fetch_add(1, Ordering::SeqCst) < WRONG_ANSWERS {
                Some("ses_somebody_else")
            } else {
                Some("ses_standin")
            }
        });
        (port, served)
    }

    /// The launcher probes past every answer naming another session and is ready only on the one
    /// naming the reported session. Accepting any answer would stop at the door's first request.
    #[test]
    fn a_door_naming_another_session_is_not_ready() {
        let _guard = binary_guard();
        let (port, served) = door_naming_another_session_first();
        let dir = tempfile::tempdir().unwrap();
        let options = options(LIVENESS_BOUND);
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
        let formation = match result {
            Ok(formation) => formation,
            Err(failure) => match only_reason(&failure) {
                MemberFailureReason::DidNotReport { .. } => panic!(
                    "the stand-in's readiness line did not arrive within {LIVENESS_BOUND:?} \
                     (load average {}); host load is the likely cause: {failure:?}",
                    load_average()
                ),
                _ => panic!(
                    "the launch was refused although the door answered as ses_standin from its \
                     request {} on: {failure:?}",
                    WRONG_ANSWERS + 1
                ),
            },
        };
        let peer = &formation.peers()[0];
        assert_eq!(peer.session_id, "ses_standin");
        let served = served.load(Ordering::SeqCst);
        assert!(
            served > WRONG_ANSWERS,
            "an answer naming another session was counted as ready: the door served {served} \
             request(s), and its first {WRONG_ANSWERS} named ses_somebody_else"
        );
        let pid = peer.pid;
        drop(formation);
        assert!(
            !crate::running::pid_is_alive(pid),
            "dropping the owner stops the peer"
        );
    }

    #[test]
    fn a_door_counts_as_ready_only_when_it_answers_as_the_reported_session() {
        let port = door("ses_somebody_else", Duration::ZERO);
        let probed = crate::running::probe_door_session_id(&format!("localhost:{port}"), None);
        match door_answers_as(probed, "ses_standin") {
            Err(why) => assert!(
                why.contains("the door answers as session ses_somebody_else"),
                "{why}"
            ),
            Ok(()) => panic!("a door answering as ses_somebody_else counted as ses_standin"),
        }
        assert_eq!(
            door_answers_as(Ok("ses_standin".to_string()), "ses_standin"),
            Ok(())
        );
        assert_eq!(
            door_answers_as(Err("connection refused".to_string()), "ses_standin"),
            Err("connection refused".to_string())
        );
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
            // `reviewer` calls no one, so it is absent; no plan has a callee that is the entry
            // member, because admission never admits an edge into it.
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
        // Every member, the entry member included, is told the one launcher that started it:
        // this test process.
        let launcher = crate::running::ProcessIdentity::of_this_process();
        assert!(launcher.is_some());
        for bundle in [&coder_bundle, &planner_bundle, &reviewer_bundle] {
            assert_eq!(bundle.formation_id, id);
            assert_eq!(bundle.verify_key, coder_bundle.verify_key);
            assert_eq!(bundle.launcher, launcher, "{}", bundle.member);
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
            workdir: PathBuf::from("/home/u/.murmur/formations/frm_x/coder"),
            door_token: Some(DoorToken::new("mdt1.secretpayload.mac".to_string())),
        };
        let debug = format!("{peer:?}");
        assert!(!debug.contains("secretpayload"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    /// A peer's directory is `<home>/formations/<frm_id>/<member>`, every level owner-only.
    #[test]
    fn a_member_directory_is_made_owner_only_under_its_formation() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let formations = home.path().join(FORMATIONS_DIR);
        let id = FormationId::mint();
        let workdir = make_member_workdir(&formations, &id, "worker").unwrap();
        assert_eq!(workdir, formations.join(id.as_str()).join("worker"));
        for dir in [&formations, &formations.join(id.as_str()), &workdir] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
    }

    /// Nothing already at a member's path becomes its preopen: a directory, a file or a symlink
    /// there refuses the member, naming the path.
    #[test]
    fn an_existing_member_path_is_refused_and_named() {
        let home = tempfile::tempdir().unwrap();
        let formations = home.path().join(FORMATIONS_DIR);
        let id = FormationId::mint();
        make_member_workdir(&formations, &id, "worker").unwrap();
        let again = make_member_workdir(&formations, &id, "worker").unwrap_err();
        let path = formations.join(id.as_str()).join("worker");
        assert!(again.contains(&path.display().to_string()), "{again}");
        std::os::unix::fs::symlink(home.path(), formations.join(id.as_str()).join("linked"))
            .unwrap();
        assert!(make_member_workdir(&formations, &id, "linked").is_err());
    }

    /// A peer whose directory cannot be made is not started, and the launch is refused naming it.
    #[test]
    fn a_peer_whose_directory_cannot_be_made_refuses_the_launch() {
        let _guard = binary_guard();
        let dir = tempfile::tempdir().unwrap();
        let options = options(Duration::from_secs(5));
        let formations = options.formations_dir.clone().unwrap();
        let taken = formations.join(options.formation_id.as_str()).join("coder");
        std::fs::create_dir_all(&taken).unwrap();
        std::env::set_var(child_launch::MUR_BINARY_ENV, stand_in(dir.path(), None));
        let result = launch_plan(plan_of("coder"), options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        let Err(failure) = result else {
            panic!("the launch was expected to be refused");
        };
        assert!(failure.stopped.is_empty(), "{failure:?}");
        match only_reason(&failure) {
            MemberFailureReason::NotStarted { error } => {
                assert!(error.contains(&taken.display().to_string()), "{error}")
            }
            other => panic!("expected NotStarted, got {other:?}"),
        }
    }

    /// A member's directory is `formations/<frm_id>/<member>`, and the roots `mur trace show
    /// frm_<id>` searches are one `.murmur` per member directory that has one, sorted, and none for
    /// a formation with no directory.
    #[test]
    fn member_roots_are_the_member_directories_that_hold_sessions() {
        assert!(formation_member_roots(&FormationId::mint()).is_empty());

        let id = FormationId::mint();
        let home = tempfile::tempdir().unwrap();
        let formations = home.path().join(FORMATIONS_DIR);
        for member in ["worker", "reviewer", "idle"] {
            assert_eq!(
                make_member_workdir(&formations, &id, member).unwrap(),
                formations.join(id.as_str()).join(member)
            );
        }
        for member in ["worker", "reviewer"] {
            std::fs::create_dir(formations.join(id.as_str()).join(member).join(".murmur")).unwrap();
        }
        assert_eq!(
            formation_member_roots_in(&formations, &id),
            [
                formations.join(id.as_str()).join("reviewer/.murmur"),
                formations.join(id.as_str()).join("worker/.murmur"),
            ]
        );
    }

    /// The marker is three keys, written and read back unchanged.
    #[test]
    fn formation_marker_round_trips_as_three_keys() {
        let marker = FormationMarker {
            project_dir: PathBuf::from("/srv/project"),
            launcher_pid: 4242,
            launcher_start: "12345".to_string(),
        };
        let value = serde_json::to_value(&marker).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "project_dir": "/srv/project",
                "launcher_pid": 4242,
                "launcher_start": "12345",
            })
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORMATION_MARKER_FILE), value.to_string()).unwrap();
        assert_eq!(FormationMarker::read(dir.path()), Some(marker));
        std::fs::write(dir.path().join(FORMATION_MARKER_FILE), "{").unwrap();
        assert_eq!(FormationMarker::read(dir.path()), None);
        assert_eq!(FormationMarker::read(&dir.path().join("missing")), None);
    }

    /// A launch with peers writes its marker at `0600` into its formation's directory before the
    /// first peer's directory is made, naming the canonical project directory and this process as
    /// its launcher — and writes it even when the launch is then refused.
    #[test]
    fn formation_marker_is_written_owner_only_before_the_first_peer_directory() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = binary_guard();
        let dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut options = options(Duration::from_secs(1));
        options.project_dir = project.path().join(".");
        let formation_dir = options
            .formations_dir
            .clone()
            .unwrap()
            .join(options.formation_id.as_str());
        std::env::set_var(child_launch::MUR_BINARY_ENV, stand_in(dir.path(), None));
        let result = launch_plan(plan_of("coder"), options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        assert!(result.is_err(), "the stand-in never reports itself");

        let path = formation_dir.join(FORMATION_MARKER_FILE);
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let marker = FormationMarker::read(&formation_dir).unwrap();
        let pid = std::process::id();
        assert_eq!(
            marker,
            FormationMarker {
                project_dir: std::fs::canonicalize(project.path()).unwrap(),
                launcher_pid: pid,
                launcher_start: crate::running::process_start_token(pid).unwrap_or_default(),
            }
        );
        let peer_dir = std::fs::metadata(formation_dir.join("coder")).unwrap();
        assert!(
            metadata.modified().unwrap() <= peer_dir.modified().unwrap(),
            "the marker was written after the peer's directory was made"
        );
    }

    /// A plan with no peers makes no formation directory, so there is nowhere for a marker.
    #[test]
    fn formation_marker_is_never_written_without_peers() {
        let _guard = binary_guard();
        let dir = tempfile::tempdir().unwrap();
        let options = options(Duration::from_secs(5));
        let formations = options.formations_dir.clone().unwrap();
        std::env::set_var(child_launch::MUR_BINARY_ENV, stand_in(dir.path(), None));
        let plan = FormationPlan {
            peers: Vec::new(),
            entry: member("planner"),
            callees: Vec::new(),
        };
        let result = launch_plan(plan, options);
        std::env::remove_var(child_launch::MUR_BINARY_ENV);
        let formation = result.expect("a formation with no peers has nothing to wait for");
        drop(formation);
        assert!(
            std::fs::read_dir(&formations).map_or(true, |mut entries| entries.next().is_none()),
            "{}",
            formations.display()
        );
    }

    fn marker_of(pid: u32, start: &str) -> FormationMarker {
        FormationMarker {
            project_dir: PathBuf::from("/srv/project"),
            launcher_pid: pid,
            launcher_start: start.to_string(),
        }
    }

    /// A pid this test started and has already reaped: no process holds it.
    fn reaped_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn running_record(pid: u32, formation: Option<&FormationId>) -> crate::running::RunningRecord {
        crate::running::RunningRecord {
            session_id: "ses_0199c4e2f1b7712a9d3e4f5061728394".to_string(),
            url: "127.0.0.1:1".to_string(),
            pid,
            process_start: crate::running::process_start_token(pid).unwrap_or_default(),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/tmp/worker"),
            outlives_launcher: false,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: None,
            credentials: Default::default(),
            formation_id: formation.cloned(),
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
        }
    }

    /// A formation is live while its launcher is — this process with its own start token — and
    /// not once its launcher's pid is held by nothing.
    #[test]
    fn prune_formations_liveness_reads_the_launcher_from_the_marker() {
        let id = FormationId::mint();
        let own = std::process::id();
        let token = crate::running::process_start_token(own).unwrap();
        assert!(formation_is_live(&id, &marker_of(own, &token), Some(&[])));
        assert!(!formation_is_live(
            &id,
            &marker_of(reaped_pid(), &token),
            Some(&[])
        ));
    }

    /// A formation whose launcher is gone is live while a running record names it and that
    /// record's process is alive; a record naming another formation, or a gone process, is not
    /// enough.
    #[test]
    fn prune_formations_liveness_reads_running_records_that_name_the_formation() {
        let id = FormationId::mint();
        let dead = marker_of(reaped_pid(), "1");
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let live = running_record(sleeper.id(), Some(&id));
        let other = running_record(sleeper.id(), Some(&FormationId::mint()));
        let unnamed = running_record(sleeper.id(), None);
        let gone = running_record(reaped_pid(), Some(&id));

        assert!(formation_is_live(&id, &dead, Some(&[live])));
        assert!(!formation_is_live(
            &id,
            &dead,
            Some(&[other, unnamed, gone])
        ));
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// A running directory that cannot be listed says nothing about what is running, so every
    /// formation is live and nothing is removed.
    #[test]
    fn prune_formations_liveness_treats_an_unlistable_running_directory_as_all_live() {
        let id = FormationId::mint();
        assert!(formation_is_live(&id, &marker_of(reaped_pid(), "1"), None));

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let formations = home.path().join(FORMATIONS_DIR);
        let old = FormationId::parse(&format!("frm_{:012x}{:020x}", 1_000_000u64, 1)).unwrap();
        let old_dir = formations.join(old.as_str());
        std::fs::create_dir_all(old_dir.join("worker")).unwrap();
        let marker = FormationMarker {
            project_dir: std::fs::canonicalize(project.path()).unwrap(),
            ..marker_of(reaped_pid(), "1")
        };
        std::fs::write(
            old_dir.join(FORMATION_MARKER_FILE),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();
        let policy = murmur_artifact::TraceRetainConfig {
            max_sessions: Some(1),
            max_age_secs: Some(1),
        };

        // A regular file where the running directory should be cannot be listed.
        let running = home.path().join("running");
        std::fs::write(&running, "").unwrap();
        let current = FormationId::mint();
        assert!(prune_ended_formations_in(
            &formations,
            &running,
            &current,
            project.path(),
            &policy
        )
        .is_empty());
        assert!(old_dir.is_dir());

        // A running directory that does not exist is an empty machine, and the ended one goes.
        std::fs::remove_file(&running).unwrap();
        let pruned =
            prune_ended_formations_in(&formations, &running, &current, project.path(), &policy);
        assert_eq!(pruned.len(), 1, "{pruned:?}");
        assert_eq!(pruned[0].formation_id, old);
        assert_eq!(pruned[0].reason, crate::trace::RETENTION_REASON_MAX_AGE);
        assert!(!old_dir.exists());
    }
}
