//! Guest workers: the processes a session's WASM guests run in.
//!
//! A `mur run` session is one supervisor process and up to three workers, one per guest kind
//! ([`WorkerKind`]), each started only when the capsule has a guest of that kind. A worker holds
//! the Wasmtime engine and every store of its kind and serves WASI filesystem, clocks, random,
//! the execution limits and the wall-clock bound itself. Every other host import is a host call
//! to the supervisor over the worker's connection, naming the supervisor-issued call it belongs
//! to; the supervisor answers it with the objects it registered for that call, and only while the
//! call is in flight on that connection.
//!
//! The supervisor keeps everything a guest must never hold: the trace and records, every
//! credential gateway and its key, formation and door tokens, the cgroup scope and the session
//! processes. No frame it sends a worker carries a key or a token.
//!
//! The wire format is in [`wire`]: `u32` little-endian length-prefixed `postcard` frames over one
//! Unix socket, which a worker process holds as fd 3.

// Raw syscalls are what the probe bodies are for; every `unsafe` block in it carries its SAFETY
// note.
#[cfg(all(feature = "escape-probe", target_os = "linux"))]
#[allow(unsafe_code)]
mod escape_probe;
mod http;
mod launch;
mod remote;
mod supervisor;
pub(crate) mod wire;
mod worker;

use std::ffi::OsString;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

use crate::errors::RuntimeError;

pub(crate) use http::RemoteHttp;
pub(crate) use launch::{Launcher, WorkerSandbox};
pub(crate) use remote::RemoteProcessDriver;
pub use supervisor::GuestWorkers;
pub(crate) use wire::StreamEvent;
pub use worker::RemoteLink;

/// The guest-worker protocol version both sides state in the handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// The hidden `mur` subcommand a worker process is started with.
pub const ENTRY: &str = "__guest-worker";

/// The three worker kinds. A guest is assigned by its artifact's `runtime:` kind, never by the
/// role of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WorkerKind {
    /// Every `runtime: driver` artifact, the `transport: process` driver and a script capsule's
    /// own component.
    Driver,
    /// Every `runtime: tool` WASM artifact.
    Tools,
    /// Every `runtime: hook` artifact.
    Hooks,
}

impl WorkerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Driver => "driver",
            Self::Tools => "tools",
            Self::Hooks => "hooks",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "driver" => Some(Self::Driver),
            "tools" => Some(Self::Tools),
            "hooks" => Some(Self::Hooks),
            _ => None,
        }
    }

    /// The worker kind that holds an artifact of `runtime`, or `None` for a skill, which runs no
    /// guest.
    pub fn for_runtime(runtime: murmur_artifact::ArtifactRuntime) -> Option<Self> {
        use murmur_artifact::ArtifactRuntime;
        match runtime {
            ArtifactRuntime::Tool => Some(Self::Tools),
            ArtifactRuntime::Driver => Some(Self::Driver),
            ArtifactRuntime::Hook => Some(Self::Hooks),
            ArtifactRuntime::Skill => None,
        }
    }

    /// The worker's async runtime threads: the tools worker serves concurrent bridged tool calls.
    fn runtime_threads(self) -> usize {
        match self {
            Self::Tools => 4,
            Self::Driver | Self::Hooks => 2,
        }
    }
}

const PLACEMENT_UNSPLIT: u8 = 0;
const PLACEMENT_SUPERVISOR: u8 = 1;
const PLACEMENT_WORKER: u8 = 2;

/// What this process is, for the store guard: set once by `mur` before any command runs.
static PLACEMENT: AtomicU8 = AtomicU8::new(PLACEMENT_UNSPLIT);

/// Where a process stands with respect to guest workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// No caller registered worker processes: a library embedder or capsule-runtime's own tests.
    Unsplit,
    /// A process that uses worker processes and is not one: `mur`.
    Supervisor,
    /// A guest worker.
    Worker,
}

/// Registers that this process runs its guests in worker processes. `mur` calls this once before
/// any command runs; from then on a store created outside a worker is refused.
pub fn use_worker_processes() {
    let _ = PLACEMENT.compare_exchange(
        PLACEMENT_UNSPLIT,
        PLACEMENT_SUPERVISOR,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
}

#[cfg(test)]
thread_local! {
    /// A placement a test puts its own thread at, ahead of the process-wide one.
    static TEST_PLACEMENT: std::cell::Cell<Option<Placement>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with this thread at `placement`, for a test that drives a store site as a supervisor
/// would without changing the process-wide placement other tests read.
#[cfg(test)]
pub(crate) fn with_placement<R>(placement: Placement, f: impl FnOnce() -> R) -> R {
    TEST_PLACEMENT.with(|cell| cell.set(Some(placement)));
    let result = f();
    TEST_PLACEMENT.with(|cell| cell.set(None));
    result
}

pub(crate) fn current_placement() -> Placement {
    #[cfg(test)]
    if let Some(placement) = TEST_PLACEMENT.with(std::cell::Cell::get) {
        return placement;
    }
    match PLACEMENT.load(Ordering::SeqCst) {
        PLACEMENT_SUPERVISOR => Placement::Supervisor,
        PLACEMENT_WORKER => Placement::Worker,
        _ => Placement::Unsplit,
    }
}

/// The launcher a session's workers are started with, or `None` when guests run in this process.
pub(crate) fn session_launcher() -> Option<Launcher> {
    match current_placement() {
        Placement::Supervisor => Some(Launcher::own_image()),
        Placement::Unsplit | Placement::Worker => None,
    }
}

/// A place in capsule-runtime that creates a Wasmtime store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreSite {
    Tool,
    Driver,
    Hook,
    ProcessDriver,
    Capsule,
}

impl StoreSite {
    fn describe(self) -> &'static str {
        match self {
            Self::Tool => "a tool",
            Self::Driver => "an inference driver",
            Self::Hook => "a hook",
            Self::ProcessDriver => "a process driver",
            Self::Capsule => "a script capsule's component",
        }
    }
}

/// Whether a store for `site` may be created in a process at `placement`: anywhere but a process
/// that uses worker processes and is not itself one.
pub(crate) fn store_allowed(placement: Placement, site: StoreSite) -> Result<(), RuntimeError> {
    match placement {
        Placement::Unsplit | Placement::Worker => Ok(()),
        Placement::Supervisor => Err(RuntimeError::Runtime(format!(
            "refused to create a store for {} in the supervisor: guests run only in guest workers",
            site.describe()
        ))),
    }
}

/// [`store_allowed`] for this process.
pub(crate) fn check_store_placement(site: StoreSite) -> Result<(), RuntimeError> {
    store_allowed(current_placement(), site)
}

/// What `mur __guest-worker …` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEntry {
    Kind(WorkerKind),
    /// `mur __guest-worker` with anything but exactly one kind.
    Misuse,
}

/// The worker entry `args` (the whole argv) asks for, or `None` when this is not a worker start.
pub fn entry_kind(args: impl IntoIterator<Item = OsString>) -> Option<WorkerEntry> {
    let mut args = args.into_iter().skip(1);
    if args.next()?.to_str() != Some(ENTRY) {
        return None;
    }
    let kind = args.next();
    if args.next().is_some() {
        return Some(WorkerEntry::Misuse);
    }
    Some(
        kind.and_then(|kind| kind.to_str().and_then(WorkerKind::parse))
            .map_or(WorkerEntry::Misuse, WorkerEntry::Kind),
    )
}

/// A worker process's whole life. Refuses a non-empty environment and a missing connection with
/// exit code 2; otherwise serves until `Shutdown` or end of file.
pub fn worker_main(entry: WorkerEntry) -> i32 {
    let kind = match entry {
        WorkerEntry::Kind(kind) => kind,
        WorkerEntry::Misuse => {
            crate::diagnostic::to_stderr(&format!(
                "mur {ENTRY} is an internal entry started by mur run; it is not a command"
            ));
            return 2;
        }
    };
    let count = std::env::vars_os().count();
    if count != 0 {
        crate::diagnostic::to_stderr(&format!(
            "mur {ENTRY} {kind}: refused to start: its environment holds {count} variable(s); a \
             guest worker starts with an empty environment",
            kind = kind.as_str()
        ));
        return 2;
    }
    let Some(stream) = connection_on_fd3() else {
        crate::diagnostic::to_stderr(&format!(
            "mur {ENTRY} {kind}: refused to start: fd 3 is not its connection; it is an internal \
             entry started by mur run",
            kind = kind.as_str()
        ));
        return 2;
    };
    PLACEMENT.store(PLACEMENT_WORKER, Ordering::SeqCst);
    worker::serve(kind, stream)
}

#[allow(unsafe_code)]
fn connection_on_fd3() -> Option<std::os::unix::net::UnixStream> {
    use std::os::fd::FromRawFd;
    // SAFETY: fstat writes into a zeroed struct we own.
    let is_socket = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        libc::fstat(3, &mut stat) == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    if !is_socket {
        return None;
    }
    // SAFETY: fd 3 is the socket the supervisor started this process with. `main` reaches this
    // before anything else runs or opens a descriptor, so nothing else owns fd 3.
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    // CLOEXEC, so no guest-spawned process could inherit it.
    // SAFETY: integer arguments only.
    unsafe {
        let flags = libc::fcntl(3, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(3, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    Some(stream)
}

#[cfg(test)]
mod tests;
