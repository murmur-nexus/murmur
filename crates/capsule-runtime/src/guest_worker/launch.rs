//! Starting a worker: a re-executed `mur` with an empty environment and its connection on fd 3,
//! or, where no caller registered worker processes, the same server on a thread.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use super::{WorkerKind, ENTRY};

/// What a worker process is confined by: the session's enforcement narrowed to the worker's
/// grant closure ([`crate::sandbox::ShellEnforcement::for_guest_worker`]) and the work folder
/// [`crate::sandbox::prepare_enforcement`] gives it read-write.
#[derive(Debug, Clone)]
pub(crate) struct WorkerSandbox {
    pub(crate) enforcement: crate::sandbox::ShellEnforcement,
    pub(crate) workdir: PathBuf,
}

impl WorkerSandbox {
    /// Whether the worker is forked into the session's PID namespace, where the process `spawn`
    /// returns is its relay and the worker knows itself by a namespace-local pid.
    pub(crate) fn enters_pid_namespace(&self) -> bool {
        !matches!(
            self.enforcement.tier,
            crate::sandbox::EnforcementTier::EnvironmentOnly
                | crate::sandbox::EnforcementTier::Unconfined
        ) && cfg!(target_os = "linux")
    }
}

/// How a session's workers are started.
#[derive(Clone)]
pub(crate) enum Launcher {
    /// Re-execute `program` (this process's own image) as `mur __guest-worker <kind>`.
    Process { program: PathBuf },
    /// Serve on a thread of this process, over a socket pair, with the same protocol.
    #[cfg(test)]
    Thread,
    /// A thread running `serve` in place of the worker server, for tests of the handshake.
    #[cfg(test)]
    ThreadServing(fn(WorkerKind, UnixStream)),
    /// The worker end of each connection, handed to the test to speak for the worker itself.
    #[cfg(test)]
    Peer(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<WorkerKind, UnixStream>>>),
}

impl Launcher {
    /// The launcher `mur` uses: this process's own image, through `/proc/self/exe` on Linux so a
    /// binary replaced on disk mid-run cannot drift from the supervisor.
    pub(crate) fn own_image() -> Self {
        #[cfg(target_os = "linux")]
        let program = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mur"));
        Self::Process { program }
    }
}

/// Starts a worker of `kind`. Returns the supervisor's end of its connection and, for a worker
/// process, the child.
pub(crate) fn start_worker(
    launcher: &Launcher,
    kind: WorkerKind,
    sandbox: Option<&WorkerSandbox>,
) -> Result<(UnixStream, Option<Child>), String> {
    let (ours, theirs) =
        UnixStream::pair().map_err(|error| format!("no socket pair for it: {error}"))?;
    match launcher {
        #[cfg(test)]
        Launcher::Thread => {
            std::thread::Builder::new()
                .name(format!("guest-worker-{}", kind.as_str()))
                .spawn(move || {
                    super::worker::serve(kind, theirs);
                })
                .map_err(|error| format!("its thread did not start: {error}"))?;
            Ok((ours, None))
        }
        #[cfg(test)]
        Launcher::ThreadServing(serve) => {
            let serve = *serve;
            std::thread::spawn(move || serve(kind, theirs));
            Ok((ours, None))
        }
        #[cfg(test)]
        Launcher::Peer(peers) => {
            peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(kind, theirs);
            Ok((ours, None))
        }
        Launcher::Process { program } => {
            let child = spawn_process(program, kind, theirs, sandbox)?;
            Ok((ours, Some(child)))
        }
    }
}

/// Spawns `mur __guest-worker <kind>` with its connection on fd 3.
///
/// With a `sandbox`, the worker is spawned through [`crate::sandbox::prepare_enforcement`], the
/// path every shell command takes, and three `pre_exec` closures run in this order:
///
/// 1. In the forked child: its own process group, `PR_SET_PDEATHSIG(SIGKILL)` tied to the
///    connection thread, and the check that the supervisor is still its parent. On a kernel tier
///    this process becomes the relay, so both carry over to it.
/// 2. The enforcement: cgroup and namespace joins, the fork into the session's PID namespace
///    (the forked process continues as the worker, with its own parent-death signal tied to the
///    relay), the private `/proc` or composed root, the fd-hygiene step, the capability drop,
///    seccomp, Landlock and the rlimits.
/// 3. The connection: the socket moved to fd 3 (`dup2`, or its close-on-exec flag cleared when it
///    is already 3) and every descriptor above 3 marked close-on-exec. This runs after the
///    fd-hygiene step, which marks every descriptor from 3 up close-on-exec, fd 3 included; it
///    uses only `dup2`/`fcntl`/`close_range`, which the seccomp baseline admits.
#[allow(unsafe_code)]
fn spawn_process(
    program: &PathBuf,
    kind: WorkerKind,
    theirs: UnixStream,
    sandbox: Option<&WorkerSandbox>,
) -> Result<Child, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let fd = theirs.as_raw_fd();
    let mut command = Command::new(program);
    // argv[0] names the binary as a person would read it in `ps`.
    if let Ok(exe) = std::env::current_exe() {
        command.arg0(exe);
    }
    command
        .args([ENTRY, kind.as_str()])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(target_os = "linux")]
    // SAFETY: getpid has no preconditions.
    let parent = unsafe { libc::getpid() };
    // SAFETY: every call below is async-signal-safe (setpgid, prctl, getppid) and touches only
    // integers.
    unsafe {
        command.pre_exec(move || {
            // Its own process group, so a terminal's SIGINT to mur's group reaches mur alone.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The supervisor may have died between fork and the prctl above.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
    let handle = match sandbox {
        Some(sandbox) => Some(crate::sandbox::prepare_enforcement(
            &mut command,
            &sandbox.enforcement,
            &sandbox.workdir,
        )?),
        None => None,
    };
    // SAFETY: every call below is async-signal-safe (fcntl, dup2, close_range) and touches only
    // this child's own descriptors.
    unsafe {
        command.pre_exec(move || {
            if fd == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            close_above_three();
            Ok(())
        });
    }
    let spawned = command.spawn();
    // The enforcement's `pre_exec` closure holds the parent's copies of the diagnostic pipe's
    // write end and of the namespace and Landlock descriptors: dropped before the diagnostic read.
    drop(command);
    let child = spawned.map_err(|error| {
        let detail = handle
            .as_ref()
            .and_then(crate::sandbox::SupervisorHandle::read_diagnostic)
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        format!("{} did not start: {error}{detail}", program.display())
    })?;
    drop(theirs);
    Ok(child)
}

/// Marks every descriptor above 3 close-on-exec (closing them where the kernel cannot mark them),
/// so the worker holds its connection and stdio and nothing else.
///
/// # Safety
/// Called only between fork and exec.
#[allow(unsafe_code)]
unsafe fn close_above_three() {
    #[cfg(target_os = "linux")]
    // SAFETY: integer arguments only.
    unsafe {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        if libc::syscall(
            libc::SYS_close_range,
            4 as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        ) == 0
        {
            return;
        }
        crate::cgroup::close_range(4, libc::c_uint::MAX);
    }
    #[cfg(not(target_os = "linux"))]
    // SAFETY: integer arguments only.
    unsafe {
        let mut fd = 4;
        while fd < 4096 {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
            fd += 1;
        }
    }
}
