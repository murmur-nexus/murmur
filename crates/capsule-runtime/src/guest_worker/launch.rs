//! Starting a worker: a re-executed `mur` with an empty environment and its connection on fd 3,
//! or, where no caller registered worker processes, the same server on a thread.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use super::{WorkerKind, ENTRY};

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
            let child = spawn_process(program, kind, theirs)?;
            Ok((ours, Some(child)))
        }
    }
}

#[allow(unsafe_code)]
fn spawn_process(program: &PathBuf, kind: WorkerKind, theirs: UnixStream) -> Result<Child, String> {
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
    // SAFETY: every call below is async-signal-safe (setpgid, prctl, getppid, fcntl, dup2,
    // close_range) and touches only integers and this child's own descriptors.
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
    let child = command
        .spawn()
        .map_err(|error| format!("{} did not start: {error}", program.display()))?;
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
