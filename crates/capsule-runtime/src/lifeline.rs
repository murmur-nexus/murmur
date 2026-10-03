//! A formation member's lifeline: one pipe per member, through which a member learns that its
//! formation has ended.
//!
//! The launcher creates the pipe, hands the member the read end, and holds the only write end. It
//! never writes to it. When the launcher ends the formation it closes the write end; when the
//! launcher's process dies by any means, `SIGKILL` and the OOM killer included, the kernel closes
//! it. Either way the member's read returns zero bytes. **EOF on a lifeline means the formation
//! has ended, and nothing else.**
//!
//! * Bytes read are discarded and are not EOF; only a zero-byte read is. Nothing writes to a
//!   lifeline, so no writer can block on a full pipe or be sent `SIGPIPE`.
//! * Both ends are `FD_CLOEXEC` from creation. The one descriptor whose flag is cleared is a
//!   member's own read end, and only in that member's forked child, by [`MemberLifeline::hand_to`],
//!   so no other process ever holds a write end that would keep a dead formation alive.
//! * A member reads [`FORMATION_LIFELINE_ENV`] once per process, through
//!   [`FormationLifeline::from_env`], which validates the descriptor, sets `FD_CLOEXEC` on it so
//!   nothing the member spawns inherits it, and takes sole ownership of it.
//! * A read error other than `EINTR` is reported and treated as EOF: a member that can no longer
//!   hear its lifeline could not be wound down by anything else once its launcher is gone.
//!
//! Unlike the formation id, which belongs to the session, the lifeline belongs to the process:
//! a descriptor is a property of the process holding it.

/// The variable a formation launcher sets in each member's environment: the decimal number of the
/// descriptor holding that member's lifeline read end. Runtime-owned: never resolved from the host
/// for a guest, a shell, a native tool or a hook, and never handed to a delegated child.
pub const FORMATION_LIFELINE_ENV: &str = "MURMUR_FORMATION_LIFELINE";

#[cfg(unix)]
pub use unix::{FormationLifeline, MemberLifeline};

#[cfg(unix)]
mod unix {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::FORMATION_LIFELINE_ENV;
    use crate::errors::RuntimeError;

    /// Set by the first [`FormationLifeline::from_env`] that finds the variable set. A second read
    /// would hand out a second owner of the same descriptor number.
    static TAKEN: AtomicBool = AtomicBool::new(false);

    /// The lowest descriptor a lifeline may be: 0, 1 and 2 are a process's standard streams, which
    /// a spawn replaces in the child.
    const FIRST_LIFELINE_FD: RawFd = 3;

    /// A member's read end of its lifeline, owned by this process alone.
    #[derive(Debug)]
    pub struct FormationLifeline {
        fd: OwnedFd,
    }

    fn unreadable(reason: impl Into<String>) -> RuntimeError {
        RuntimeError::FormationLifelineUnreadable {
            reason: reason.into(),
        }
    }

    impl FormationLifeline {
        /// This process's lifeline, read from [`FORMATION_LIFELINE_ENV`].
        ///
        /// `Ok(None)` when the variable is absent or blank. Otherwise the value must be the decimal
        /// number, 3 or above, of an open descriptor that is a FIFO opened read-only; anything else
        /// is an error, as is a second read of a set variable in the same process. On success the
        /// descriptor has `FD_CLOEXEC` set and is owned by the returned value.
        pub fn from_env() -> Result<Option<Self>, RuntimeError> {
            Self::from_env_once(&TAKEN)
        }

        #[allow(unsafe_code)]
        pub(crate) fn from_env_once(taken: &AtomicBool) -> Result<Option<Self>, RuntimeError> {
            let Some(raw) = std::env::var_os(FORMATION_LIFELINE_ENV) else {
                return Ok(None);
            };
            let raw = raw.to_string_lossy();
            let value = raw.trim();
            if value.is_empty() {
                return Ok(None);
            }
            if taken.swap(true, Ordering::SeqCst) {
                return Err(unreadable(
                    "it was already read by this process, and a descriptor has one owner",
                ));
            }
            let fd = parse_fd(value)?;
            check_read_end(fd)?;
            set_cloexec(fd).map_err(|error| {
                unreadable(format!(
                    "descriptor {fd} could not be made close-on-exec: {error}"
                ))
            })?;
            // SAFETY: `fd` is open (`check_read_end` read its flags and status), and this is the
            // one read of the variable in this process (`taken`), so no other owner of the number
            // was created from it. The launcher hands each member a descriptor nothing else in the
            // member opened, so ownership passes here and nowhere else.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Ok(Some(Self { fd }))
        }

        /// Hand the lifeline to a detached thread that blocks on it and calls `on_closed` once,
        /// at EOF. EOF that came before this call is seen at once.
        ///
        /// The thread owns the descriptor from here on; nothing else in the process holds or
        /// closes it. Errors only when the thread cannot be started.
        pub fn watch(self, on_closed: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
            std::thread::Builder::new()
                .name("formation-lifeline".to_string())
                .spawn(move || {
                    wait_for_eof(self.fd);
                    on_closed();
                })
                .map(|_| ())
        }
    }

    /// Block until `fd` reads EOF, discarding any bytes and retrying `EINTR`.
    fn wait_for_eof(fd: OwnedFd) {
        use std::io::Read;
        let mut pipe = std::fs::File::from(fd);
        let mut buffer = [0u8; 64];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    crate::runtime_err!(
                        "[capsule-runtime] the formation lifeline could not be read ({error}); \
                         treating the formation as ended"
                    );
                    return;
                }
            }
        }
    }

    fn parse_fd(value: &str) -> Result<RawFd, RuntimeError> {
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(unreadable("it is not a decimal descriptor number"));
        }
        let fd: RawFd = value
            .parse()
            .map_err(|_| unreadable("it is not a descriptor number this platform has"))?;
        if fd < FIRST_LIFELINE_FD {
            return Err(unreadable(format!(
                "descriptor {fd} is a standard stream, and a lifeline is {FIRST_LIFELINE_FD} or above"
            )));
        }
        Ok(fd)
    }

    /// Refuse `fd` unless it is open, a FIFO, and opened read-only.
    #[allow(unsafe_code)]
    fn check_read_end(fd: RawFd) -> Result<(), RuntimeError> {
        // SAFETY: `F_GETFL` takes no third argument and dereferences nothing; an unopened `fd`
        // fails with `EBADF`.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(unreadable(format!("descriptor {fd} is not open")));
        }
        // SAFETY: `stat` is a zeroed plain-data struct `fstat` fills in; `fd` is open.
        let mode = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) != 0 {
                return Err(unreadable(format!(
                    "descriptor {fd} could not be examined: {}",
                    std::io::Error::last_os_error()
                )));
            }
            stat.st_mode
        };
        if mode & libc::S_IFMT != libc::S_IFIFO {
            let kind = match mode & libc::S_IFMT {
                libc::S_IFCHR => "a character device",
                libc::S_IFREG => "a regular file",
                libc::S_IFDIR => "a directory",
                libc::S_IFSOCK => "a socket",
                _ => "not a pipe",
            };
            return Err(unreadable(format!(
                "descriptor {fd} is {kind}, and a lifeline is a pipe's read end"
            )));
        }
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(unreadable(format!(
                "descriptor {fd} is open for writing, and a member holds only a read end"
            )));
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
        // SAFETY: `F_GETFD`/`F_SETFD` take and return integers and dereference nothing; `fd` is
        // open.
        let set = unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == 0
        };
        if set {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The launcher's side of one member's lifeline: the write end it holds for the formation's
    /// life, and the read end until the member has been spawned with it.
    ///
    /// Dropping it closes whatever it still holds, so a launcher that is gone has closed every
    /// lifeline.
    #[derive(Debug)]
    pub struct MemberLifeline {
        read: Option<OwnedFd>,
        write: Option<OwnedFd>,
    }

    impl MemberLifeline {
        /// A new pipe, both ends `FD_CLOEXEC`, the read end numbered 3 or above.
        ///
        /// Where `pipe2` is missing the flag is set after `pipe` returns, so a process that spawns
        /// from other threads must create every lifeline before its first spawn.
        #[allow(unsafe_code)]
        pub fn new() -> std::io::Result<Self> {
            let [read, write] = cloexec_pipe()?;
            // A process started with its standard input closed is handed descriptor 0 by `pipe`,
            // and a spawn replaces a child's 0, 1 and 2.
            let read = if read.as_raw_fd() < FIRST_LIFELINE_FD {
                // SAFETY: `F_DUPFD_CLOEXEC` takes an integer and dereferences nothing; `read` is
                // open and stays owned by its `OwnedFd`, which is dropped below.
                let duplicate = unsafe {
                    libc::fcntl(read.as_raw_fd(), libc::F_DUPFD_CLOEXEC, FIRST_LIFELINE_FD)
                };
                if duplicate < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: `duplicate` was just returned by `fcntl` and is owned by nothing else.
                unsafe { OwnedFd::from_raw_fd(duplicate) }
            } else {
                read
            };
            Ok(Self {
                read: Some(read),
                write: Some(write),
            })
        }

        /// The `(name, value)` pair naming the read end in the member's environment, while the
        /// read end is still held.
        fn env_pair(&self) -> Option<(&'static str, String)> {
            self.read
                .as_ref()
                .map(|read| (FORMATION_LIFELINE_ENV, read.as_raw_fd().to_string()))
        }

        /// Give the member `command` will start its read end: set [`FORMATION_LIFELINE_ENV`] and
        /// register a `pre_exec` that clears `FD_CLOEXEC` on that one descriptor in the child.
        ///
        /// Register it after any fd-hygiene `pre_exec`, which marks every inherited descriptor
        /// close-on-exec; `pre_exec` closures run in registration order. The child never gets the
        /// write end, which stays close-on-exec. Does nothing once [`Self::spawned`] has run.
        #[allow(unsafe_code)]
        pub fn hand_to(&self, command: &mut std::process::Command) {
            use std::os::unix::process::CommandExt;
            let Some((name, value)) = self.env_pair() else {
                return;
            };
            let fd = self.read.as_ref().map_or(-1, AsRawFd::as_raw_fd);
            command.env(name, value);
            // SAFETY: the closure runs in the forked child between `fork` and `exec`, where only
            // async-signal-safe calls are allowed. It makes two `fcntl` calls on an integer it
            // captured by value: no allocation, no lock, no reference into the parent.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        /// Close the launcher's copy of the read end once the member holds its own, so the member
        /// is the read end's only holder.
        pub fn spawned(&mut self) {
            self.read = None;
        }

        /// Close the write end: the member reads EOF, which tells it the formation has ended.
        /// Idempotent.
        pub fn close(&mut self) {
            self.write = None;
        }

        /// The write end's descriptor number, while it is open.
        pub fn write_fd(&self) -> Option<RawFd> {
            self.write.as_ref().map(AsRawFd::as_raw_fd)
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[allow(unsafe_code)]
    fn cloexec_pipe() -> std::io::Result<[OwnedFd; 2]> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a live two-element array `pipe2` writes the two new descriptors into.
        let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if created != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `pipe2` succeeded, so both numbers are open descriptors owned by nothing else.
        Ok(unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    #[allow(unsafe_code)]
    fn cloexec_pipe() -> std::io::Result<[OwnedFd; 2]> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a live two-element array `pipe` writes the two new descriptors into.
        let created = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if created != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `pipe` succeeded, so both numbers are open descriptors owned by nothing else.
        let ends = unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] };
        for end in &ends {
            set_cloexec(end.as_raw_fd())?;
        }
        Ok(ends)
    }

    #[cfg(test)]
    #[allow(unsafe_code)]
    mod tests {
        use super::*;
        use std::sync::mpsc;
        use std::time::Duration;

        /// Serializes the tests that set [`FORMATION_LIFELINE_ENV`] in this test binary's
        /// process environment.
        fn env_guard() -> std::sync::MutexGuard<'static, ()> {
            crate::formation::FORMATION_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn fd_flags(fd: RawFd) -> libc::c_int {
            // SAFETY: `F_GETFD` takes and returns integers.
            unsafe { libc::fcntl(fd, libc::F_GETFD) }
        }

        /// Read the lifeline the variable names, with a fresh once-flag standing in for the
        /// process-wide one.
        fn read_with(
            value: &str,
            taken: &AtomicBool,
        ) -> Result<Option<FormationLifeline>, RuntimeError> {
            std::env::set_var(FORMATION_LIFELINE_ENV, value);
            let read = FormationLifeline::from_env_once(taken);
            std::env::remove_var(FORMATION_LIFELINE_ENV);
            read
        }

        /// A pipe whose read end has been released by its `MemberLifeline`, as a member's would be:
        /// the descriptor number, and the launcher side holding only the write end.
        fn member_side() -> (RawFd, MemberLifeline) {
            let mut lifeline = MemberLifeline::new().unwrap();
            let read = lifeline.read.take().unwrap();
            let fd = std::os::fd::IntoRawFd::into_raw_fd(read);
            (fd, lifeline)
        }

        #[test]
        fn lifeline_from_env_sets_cloexec_and_a_second_read_is_refused() {
            let _guard = env_guard();
            let (fd, _launcher) = member_side();
            // SAFETY: `F_SETFD` on a descriptor this test owns.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            assert_eq!(fd_flags(fd) & libc::FD_CLOEXEC, 0);
            let taken = AtomicBool::new(false);
            let lifeline = read_with(&fd.to_string(), &taken).unwrap().unwrap();
            assert_eq!(lifeline.fd.as_raw_fd(), fd);
            assert_ne!(fd_flags(fd) & libc::FD_CLOEXEC, 0);

            let again = read_with(&fd.to_string(), &taken).unwrap_err().to_string();
            assert!(again.contains(FORMATION_LIFELINE_ENV), "{again}");
            assert!(again.contains("already read"), "{again}");
        }

        #[test]
        fn lifeline_absent_or_blank_is_none() {
            let _guard = env_guard();
            std::env::remove_var(FORMATION_LIFELINE_ENV);
            let taken = AtomicBool::new(false);
            assert!(FormationLifeline::from_env_once(&taken).unwrap().is_none());
            assert!(read_with("  ", &taken).unwrap().is_none());
            assert!(
                !taken.load(Ordering::SeqCst),
                "an absent lifeline takes nothing"
            );
        }

        #[test]
        fn lifeline_values_that_are_not_a_read_end_are_refused() {
            let _guard = env_guard();
            let file = tempfile::tempfile().unwrap();
            let null = std::fs::File::open("/dev/null").unwrap();
            let launcher = MemberLifeline::new().unwrap();
            let write = launcher.write_fd().unwrap();
            // The highest descriptor this process may have: other test threads open and close
            // descriptors concurrently, and the kernel hands out the lowest free number, so a
            // number just closed here could be reused before it is read.
            let closed = {
                // SAFETY: `rlimit` is a zeroed plain-data struct `getrlimit` fills in.
                let limit = unsafe {
                    let mut limit: libc::rlimit = std::mem::zeroed();
                    assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
                    limit.rlim_cur
                };
                let fd = RawFd::try_from(limit.saturating_sub(1)).unwrap_or(RawFd::MAX);
                assert!(fd_flags(fd) < 0, "descriptor {fd} is open");
                fd
            };
            for (value, expected) in [
                ("three".to_string(), "not a decimal descriptor number"),
                ("-4".to_string(), "not a decimal descriptor number"),
                ("2".to_string(), "standard stream"),
                (closed.to_string(), "is not open"),
                (null.as_raw_fd().to_string(), "a character device"),
                (file.as_raw_fd().to_string(), "a regular file"),
                (write.to_string(), "open for writing"),
            ] {
                let taken = AtomicBool::new(false);
                let refused = read_with(&value, &taken).unwrap_err().to_string();
                assert!(refused.contains(FORMATION_LIFELINE_ENV), "{refused}");
                assert!(refused.contains(expected), "{value}: {refused}");
            }
        }

        #[test]
        fn lifeline_hand_to_gives_a_child_its_read_end_and_no_write_end() {
            let mut lifeline = MemberLifeline::new().unwrap();
            let (read, write) = (
                lifeline.read.as_ref().unwrap().as_raw_fd(),
                lifeline.write_fd().unwrap(),
            );
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(format!(
                "echo \"$MURMUR_FORMATION_LIFELINE\"; \
                 if [ -e /dev/fd/{read} ]; then echo read; fi; \
                 if [ -e /dev/fd/{write} ]; then echo write; fi"
            ));
            command.stdout(std::process::Stdio::piped());
            lifeline.hand_to(&mut command);
            let child = command.spawn().unwrap();
            lifeline.spawned();
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(
                stdout.lines().collect::<Vec<_>>(),
                [&read.to_string(), "read"]
            );
            assert!(
                lifeline.env_pair().is_none(),
                "the launcher released its read end"
            );
            assert!(lifeline.write_fd().is_some());
        }

        #[test]
        fn lifeline_closing_the_write_end_fires_the_watcher_once() {
            let _guard = env_guard();
            let (fd, mut launcher) = member_side();
            let taken = AtomicBool::new(false);
            let lifeline = read_with(&fd.to_string(), &taken).unwrap().unwrap();
            let (tx, rx) = mpsc::channel();
            lifeline.watch(move || tx.send(()).unwrap()).unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            launcher.close();
            rx.recv_timeout(Duration::from_secs(5))
                .expect("EOF fires the watcher");
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_millis(300)),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                ),
                "the watcher fires once and is gone"
            );
        }

        #[test]
        fn lifeline_eof_before_watch_fires_at_once() {
            let _guard = env_guard();
            let (fd, mut launcher) = member_side();
            launcher.close();
            let taken = AtomicBool::new(false);
            let lifeline = read_with(&fd.to_string(), &taken).unwrap().unwrap();
            let (tx, rx) = mpsc::channel();
            lifeline.watch(move || tx.send(()).unwrap()).unwrap();
            rx.recv_timeout(Duration::from_secs(5))
                .expect("sticky EOF fires the watcher");
        }

        #[test]
        fn lifeline_bytes_with_the_write_end_open_do_not_fire_the_watcher() {
            let _guard = env_guard();
            let (fd, mut launcher) = member_side();
            let taken = AtomicBool::new(false);
            let lifeline = read_with(&fd.to_string(), &taken).unwrap().unwrap();
            let (tx, rx) = mpsc::channel();
            lifeline.watch(move || tx.send(()).unwrap()).unwrap();
            let bytes = [7u8; 64];
            // SAFETY: writes from a live 64-byte array to a write end this test holds.
            let written = unsafe {
                libc::write(
                    launcher.write_fd().unwrap(),
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                )
            };
            assert_eq!(written, 64);
            assert!(
                rx.recv_timeout(Duration::from_millis(500)).is_err(),
                "bytes are not EOF"
            );
            launcher.close();
            rx.recv_timeout(Duration::from_secs(5))
                .expect("EOF after the bytes fires");
        }
    }
}
