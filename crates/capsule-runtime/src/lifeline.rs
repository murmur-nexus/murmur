//! Lifelines: pipes through which a process learns that another process it depends on has ended.
//!
//! There are two, and each has one meaning:
//!
//! | Variable | Created by | EOF means |
//! |---|---|---|
//! | [`FORMATION_LIFELINE_ENV`] | the formation launcher, once per member | the formation has ended |
//! | [`SPAWNER_LIFELINE_ENV`] | a parent's runtime, once per delegated child | the process that spawned this one has ended |
//!
//! The creator hands the started process the read end and holds the only write end. It never
//! writes to it. When the creator closes the write end, or its process dies by any means —
//! `SIGKILL` and the OOM killer included — the kernel closes it, and the holder's read returns
//! zero bytes. The two variables are never interchangeable: a delegated child is never handed its
//! parent's formation lifeline, because EOF on that would mean the launcher had died, not the
//! parent.
//!
//! * Bytes read are discarded and are not EOF; only a zero-byte read is. Nothing writes to a
//!   lifeline, so no writer can block on a full pipe or be sent `SIGPIPE`.
//! * Both ends are `FD_CLOEXEC` from creation. The one descriptor whose flag is cleared is the
//!   started process's own read end, and only in that process's forked child, by
//!   [`MemberLifeline::hand_to`] or [`ChildLifeline::hand_to`], so no other process ever holds a
//!   write end that would keep a dead creator looking alive.
//! * A holder reads its variable once per process, through [`FormationLifeline::from_env`] or
//!   [`SpawnerLifeline::from_env`], each with a once-flag of its own. The read validates the
//!   descriptor, sets `FD_CLOEXEC` on it so nothing the holder spawns inherits it, and takes sole
//!   ownership of it.
//! * A read error other than `EINTR` is reported and treated as EOF: a holder that can no longer
//!   hear its lifeline could not be wound down by anything else once its creator is gone.
//!
//! Unlike the formation id, which belongs to the session, a lifeline belongs to the process: a
//! descriptor is a property of the process holding it.
//!
//! Where `pipe2` is missing (darwin), `FD_CLOEXEC` is set after `pipe` returns, so a process
//! another thread spawns in that instant can inherit a write end and delay that one holder's EOF
//! until it exits.

/// The variable a formation launcher sets in each member's environment: the decimal number of the
/// descriptor holding that member's lifeline read end. Runtime-owned: never resolved from the host
/// for a guest, a shell, a native tool or a hook, and never handed to a delegated child.
pub const FORMATION_LIFELINE_ENV: &str = "MURMUR_FORMATION_LIFELINE";

/// The variable a parent's runtime sets in each delegated child's environment, beside
/// `MURMUR_SPAWNER`: the decimal number of the descriptor holding that child's spawner lifeline
/// read end. Runtime-owned on the same terms as [`FORMATION_LIFELINE_ENV`], and never copied from
/// a parent's environment into its child's: each child is handed a pipe of its own.
pub const SPAWNER_LIFELINE_ENV: &str = "MURMUR_SPAWNER_LIFELINE";

#[cfg(unix)]
pub use unix::{ChildLifeline, FormationLifeline, MemberLifeline, SpawnerLifeline};

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{FORMATION_LIFELINE_ENV, SPAWNER_LIFELINE_ENV};
    use crate::errors::RuntimeError;

    /// Set by the first [`FormationLifeline::from_env`] that finds its variable set. A second read
    /// would hand out a second owner of the same descriptor number.
    static FORMATION_TAKEN: AtomicBool = AtomicBool::new(false);

    /// [`FORMATION_TAKEN`] for [`SpawnerLifeline::from_env`]. Separate, because a formation
    /// member's child holds no formation lifeline, and a member nobody delegated holds no spawner
    /// lifeline, but a process may in principle hold one of each.
    static SPAWNER_TAKEN: AtomicBool = AtomicBool::new(false);

    /// The lowest descriptor a lifeline may be: 0, 1 and 2 are a process's standard streams, which
    /// a spawn replaces in the child.
    const FIRST_LIFELINE_FD: RawFd = 3;

    /// What differs between the two lifelines: the variable, the error, and the words their
    /// diagnostics use. Everything else is one mechanism.
    struct Kind {
        /// The name of the thread that blocks on the read end.
        thread: &'static str,
        /// Which lifeline this is, as a diagnostic names it.
        name: &'static str,
        /// Who holds the read end, as a refusal names them.
        holder: &'static str,
        /// What EOF says has ended, as a diagnostic names it.
        ended: &'static str,
        unreadable: fn(String) -> RuntimeError,
    }

    static FORMATION: Kind = Kind {
        thread: "formation-lifeline",
        name: "formation",
        holder: "a member",
        ended: "the formation",
        unreadable: |reason| RuntimeError::FormationLifelineUnreadable { reason },
    };

    static SPAWNER: Kind = Kind {
        thread: "spawner-lifeline",
        name: "spawner",
        holder: "a child",
        ended: "the session that delegated to this one",
        unreadable: |reason| RuntimeError::SpawnerLifelineUnreadable { reason },
    };

    impl Kind {
        fn refuse(&self, reason: impl Into<String>) -> RuntimeError {
            (self.unreadable)(reason.into())
        }

        /// The read end `value` names, validated and owned; `Ok(None)` for an absent or blank
        /// value.
        ///
        /// Otherwise the value must be the decimal number, 3 or above, of an open descriptor that
        /// is a FIFO opened read-only; anything else is an error, as is a second read of a set
        /// variable under the same `taken`. On success the descriptor has `FD_CLOEXEC` set.
        #[allow(unsafe_code)]
        fn read_end(
            &self,
            value: Option<OsString>,
            taken: &AtomicBool,
        ) -> Result<Option<OwnedFd>, RuntimeError> {
            let Some(raw) = value else {
                return Ok(None);
            };
            let raw = raw.to_string_lossy();
            let value = raw.trim();
            if value.is_empty() {
                return Ok(None);
            }
            if taken.swap(true, Ordering::SeqCst) {
                return Err(self.refuse(
                    "it was already read by this process, and a descriptor has one owner",
                ));
            }
            let fd = self.parse_fd(value)?;
            self.check_read_end(fd)?;
            set_cloexec(fd).map_err(|error| {
                self.refuse(format!(
                    "descriptor {fd} could not be made close-on-exec: {error}"
                ))
            })?;
            // SAFETY: `fd` is open (`check_read_end` read its flags and status), and this is the
            // one read of the variable in this process (`taken`), so no other owner of the number
            // was created from it. The creator hands each process a descriptor nothing else in
            // that process opened, so ownership passes here and nowhere else.
            Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        fn parse_fd(&self, value: &str) -> Result<RawFd, RuntimeError> {
            if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(self.refuse("it is not a decimal descriptor number"));
            }
            let fd: RawFd = value
                .parse()
                .map_err(|_| self.refuse("it is not a descriptor number this platform has"))?;
            if fd < FIRST_LIFELINE_FD {
                return Err(self.refuse(format!(
                    "descriptor {fd} is a standard stream, and a lifeline is {FIRST_LIFELINE_FD} or above"
                )));
            }
            Ok(fd)
        }

        /// Refuse `fd` unless it is open, a FIFO, and opened read-only.
        #[allow(unsafe_code)]
        fn check_read_end(&self, fd: RawFd) -> Result<(), RuntimeError> {
            // SAFETY: `F_GETFL` takes no third argument and dereferences nothing; an unopened `fd`
            // fails with `EBADF`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(self.refuse(format!("descriptor {fd} is not open")));
            }
            // SAFETY: `stat` is a zeroed plain-data struct `fstat` fills in; `fd` is open.
            let mode = unsafe {
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut stat) != 0 {
                    return Err(self.refuse(format!(
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
                return Err(self.refuse(format!(
                    "descriptor {fd} is {kind}, and a lifeline is a pipe's read end"
                )));
            }
            if flags & libc::O_ACCMODE != libc::O_RDONLY {
                return Err(self.refuse(format!(
                    "descriptor {fd} is open for writing, and {} holds only a read end",
                    self.holder
                )));
            }
            Ok(())
        }

        /// Hand `fd` to a detached thread that blocks on it and calls `on_closed` once, at EOF.
        /// EOF that came before this call is seen at once. Errors only when the thread cannot be
        /// started.
        fn watch(
            &'static self,
            fd: OwnedFd,
            on_closed: impl FnOnce() + Send + 'static,
        ) -> std::io::Result<()> {
            std::thread::Builder::new()
                .name(self.thread.to_string())
                .spawn(move || {
                    self.wait_for_eof(fd);
                    on_closed();
                })
                .map(|_| ())
        }

        /// Block until `fd` reads EOF, discarding any bytes and retrying `EINTR`.
        fn wait_for_eof(&self, fd: OwnedFd) {
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
                            "[capsule-runtime] the {} lifeline could not be read ({error}); \
                             treating {} as ended",
                            self.name,
                            self.ended
                        );
                        return;
                    }
                }
            }
        }
    }

    /// A member's read end of its formation lifeline, owned by this process alone.
    #[derive(Debug)]
    pub struct FormationLifeline {
        fd: OwnedFd,
    }

    impl FormationLifeline {
        /// This process's formation lifeline, read from [`FORMATION_LIFELINE_ENV`].
        ///
        /// `Ok(None)` when the variable is absent or blank. Otherwise the value must be the decimal
        /// number, 3 or above, of an open descriptor that is a FIFO opened read-only; anything else
        /// is an error, as is a second read of a set variable in the same process. On success the
        /// descriptor has `FD_CLOEXEC` set and is owned by the returned value.
        pub fn from_env() -> Result<Option<Self>, RuntimeError> {
            Self::from_env_once(&FORMATION_TAKEN)
        }

        pub(crate) fn from_env_once(taken: &AtomicBool) -> Result<Option<Self>, RuntimeError> {
            let value = std::env::var_os(FORMATION_LIFELINE_ENV);
            Ok(FORMATION.read_end(value, taken)?.map(|fd| Self { fd }))
        }

        /// Hand the lifeline to a detached thread that blocks on it and calls `on_closed` once,
        /// at EOF. EOF that came before this call is seen at once.
        ///
        /// The thread owns the descriptor from here on; nothing else in the process holds or
        /// closes it. Errors only when the thread cannot be started.
        pub fn watch(self, on_closed: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
            FORMATION.watch(self.fd, on_closed)
        }
    }

    /// A delegated child's read end of its spawner lifeline, owned by this process alone. EOF on
    /// it means the process that spawned this one has ended.
    #[derive(Debug)]
    pub struct SpawnerLifeline {
        fd: OwnedFd,
    }

    impl SpawnerLifeline {
        /// This process's spawner lifeline, read from [`SPAWNER_LIFELINE_ENV`].
        ///
        /// Validated on exactly the terms of [`FormationLifeline::from_env`], under a once-flag of
        /// its own; errors are [`RuntimeError::SpawnerLifelineUnreadable`].
        pub fn from_env() -> Result<Option<Self>, RuntimeError> {
            Self::from_env_once(&SPAWNER_TAKEN)
        }

        pub(crate) fn from_env_once(taken: &AtomicBool) -> Result<Option<Self>, RuntimeError> {
            let value = std::env::var_os(SPAWNER_LIFELINE_ENV);
            Ok(SPAWNER.read_end(value, taken)?.map(|fd| Self { fd }))
        }

        /// Hand the lifeline to a detached `spawner-lifeline` thread that blocks on it and calls
        /// `on_closed` once, at EOF. EOF that came before this call is seen at once.
        ///
        /// The thread owns the descriptor from here on. Errors only when the thread cannot be
        /// started.
        pub fn watch(self, on_closed: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
            SPAWNER.watch(self.fd, on_closed)
        }
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

    /// The creator's side of one lifeline: the write end, and the read end until the process it
    /// is for has been spawned with it. Dropping it closes whatever it still holds.
    #[derive(Debug)]
    struct PipeEnds {
        read: Option<OwnedFd>,
        write: Option<OwnedFd>,
    }

    impl PipeEnds {
        /// A new pipe, both ends `FD_CLOEXEC`, the read end numbered 3 or above.
        #[allow(unsafe_code)]
        fn new() -> std::io::Result<Self> {
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

        /// The `(name, value)` pair naming the read end, while it is still held.
        fn env_pair(&self, name: &'static str) -> Option<(&'static str, String)> {
            self.read
                .as_ref()
                .map(|read| (name, read.as_raw_fd().to_string()))
        }

        /// Set `name` to the read end's number and register a `pre_exec` that clears
        /// `FD_CLOEXEC` on that one descriptor in the child. Does nothing once the read end has
        /// been released.
        #[allow(unsafe_code)]
        fn hand_to(&self, name: &'static str, command: &mut std::process::Command) {
            use std::os::unix::process::CommandExt;
            let Some((name, value)) = self.env_pair(name) else {
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

        fn write_fd(&self) -> Option<RawFd> {
            self.write.as_ref().map(AsRawFd::as_raw_fd)
        }
    }

    /// The launcher's side of one member's lifeline: the write end it holds for the formation's
    /// life, and the read end until the member has been spawned with it.
    ///
    /// Dropping it closes whatever it still holds, so a launcher that is gone has closed every
    /// lifeline.
    #[derive(Debug)]
    pub struct MemberLifeline {
        ends: PipeEnds,
    }

    impl MemberLifeline {
        /// A new pipe, both ends `FD_CLOEXEC`, the read end numbered 3 or above.
        ///
        /// Where `pipe2` is missing the flag is set after `pipe` returns, so a process that spawns
        /// from other threads must create every lifeline before its first spawn.
        pub fn new() -> std::io::Result<Self> {
            Ok(Self {
                ends: PipeEnds::new()?,
            })
        }

        /// Give the member `command` will start its read end: set [`FORMATION_LIFELINE_ENV`] and
        /// register a `pre_exec` that clears `FD_CLOEXEC` on that one descriptor in the child.
        ///
        /// Register it after any fd-hygiene `pre_exec`, which marks every inherited descriptor
        /// close-on-exec; `pre_exec` closures run in registration order. The child never gets the
        /// write end, which stays close-on-exec. Does nothing once [`Self::spawned`] has run.
        pub fn hand_to(&self, command: &mut std::process::Command) {
            self.ends.hand_to(FORMATION_LIFELINE_ENV, command);
        }

        /// Close the launcher's copy of the read end once the member holds its own, so the member
        /// is the read end's only holder.
        pub fn spawned(&mut self) {
            self.ends.read = None;
        }

        /// Close the write end: the member reads EOF, which tells it the formation has ended.
        /// Idempotent.
        pub fn close(&mut self) {
            self.ends.write = None;
        }

        /// The write end's descriptor number, while it is open.
        pub fn write_fd(&self) -> Option<RawFd> {
            self.ends.write_fd()
        }
    }

    /// A parent runtime's side of one delegated child's spawner lifeline: the write end it holds
    /// for as long as the child is its to end, and the read end until the child has been spawned
    /// with it.
    ///
    /// Dropping it closes whatever it still holds; so does this process ending, by any means.
    #[derive(Debug)]
    pub struct ChildLifeline {
        ends: PipeEnds,
    }

    impl ChildLifeline {
        /// A new pipe, both ends `FD_CLOEXEC`, the read end numbered 3 or above.
        ///
        /// Where `pipe2` is missing the flag is set after `pipe` returns, so a process another
        /// thread spawns in that instant can inherit a write end and hold that child's EOF back
        /// until it exits.
        pub fn new() -> std::io::Result<Self> {
            Ok(Self {
                ends: PipeEnds::new()?,
            })
        }

        /// Give the child `command` will start its read end: set [`SPAWNER_LIFELINE_ENV`] and
        /// register a `pre_exec` that clears `FD_CLOEXEC` on that one descriptor in the child.
        ///
        /// The `pre_exec` makes two `fcntl` calls and nothing else; register it last. The child
        /// never gets the write end, which stays close-on-exec. Does nothing once
        /// [`Self::spawned`] has run.
        pub fn hand_to(&self, command: &mut std::process::Command) {
            self.ends.hand_to(SPAWNER_LIFELINE_ENV, command);
        }

        /// The `(name, value)` pair [`Self::hand_to`] sets, while the read end is still held.
        pub fn env_pair(&self) -> Option<(&'static str, String)> {
            self.ends.env_pair(SPAWNER_LIFELINE_ENV)
        }

        /// Close this process's copy of the read end once the child holds its own, so the child
        /// is the read end's only holder.
        pub fn spawned(&mut self) {
            self.ends.read = None;
        }

        /// Close the write end: the child reads EOF, which tells it its spawner has ended.
        /// Idempotent.
        pub fn close(&mut self) {
            self.ends.write = None;
        }

        /// Keep the write end open for the rest of this process's life and let this value go.
        ///
        /// The descriptor is leaked: nothing in this process closes it again, so the
        /// child reads EOF exactly when this process exits, by whatever means.
        pub fn hold_until_exit(mut self) {
            if let Some(write) = self.ends.write.take() {
                let _ = write.into_raw_fd();
            }
        }

        /// The write end's descriptor number, while it is open.
        pub fn write_fd(&self) -> Option<RawFd> {
            self.ends.write_fd()
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

        /// Serializes the tests that set a lifeline variable in this test binary's process
        /// environment.
        fn env_guard() -> std::sync::MutexGuard<'static, ()> {
            crate::formation::FORMATION_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn fd_flags(fd: RawFd) -> libc::c_int {
            // SAFETY: `F_GETFD` takes and returns integers.
            unsafe { libc::fcntl(fd, libc::F_GETFD) }
        }

        /// Read the formation lifeline the variable names, with a fresh once-flag standing in for
        /// the process-wide one.
        fn read_with(
            value: &str,
            taken: &AtomicBool,
        ) -> Result<Option<FormationLifeline>, RuntimeError> {
            std::env::set_var(FORMATION_LIFELINE_ENV, value);
            let read = FormationLifeline::from_env_once(taken);
            std::env::remove_var(FORMATION_LIFELINE_ENV);
            read
        }

        /// [`read_with`] for the spawner lifeline.
        fn read_spawner_with(
            value: &str,
            taken: &AtomicBool,
        ) -> Result<Option<SpawnerLifeline>, RuntimeError> {
            std::env::set_var(SPAWNER_LIFELINE_ENV, value);
            let read = SpawnerLifeline::from_env_once(taken);
            std::env::remove_var(SPAWNER_LIFELINE_ENV);
            read
        }

        /// A pipe whose read end has been released by its creator, as a started process's would
        /// be: the descriptor number, and the creator side holding only the write end.
        fn released(ends: &mut PipeEnds) -> RawFd {
            ends.read.take().unwrap().into_raw_fd()
        }

        fn member_side() -> (RawFd, MemberLifeline) {
            let mut lifeline = MemberLifeline::new().unwrap();
            (released(&mut lifeline.ends), lifeline)
        }

        fn child_side() -> (RawFd, ChildLifeline) {
            let mut lifeline = ChildLifeline::new().unwrap();
            (released(&mut lifeline.ends), lifeline)
        }

        /// The highest descriptor this process may have: other test threads open and close
        /// descriptors concurrently, and the kernel hands out the lowest free number, so a number
        /// just closed here could be reused before it is read.
        fn closed_fd() -> RawFd {
            // SAFETY: `rlimit` is a zeroed plain-data struct `getrlimit` fills in.
            let limit = unsafe {
                let mut limit: libc::rlimit = std::mem::zeroed();
                assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
                limit.rlim_cur
            };
            let fd = RawFd::try_from(limit.saturating_sub(1)).unwrap_or(RawFd::MAX);
            assert!(fd_flags(fd) < 0, "descriptor {fd} is open");
            fd
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
        fn spawner_lifeline_from_env_sets_cloexec_and_a_second_read_is_refused() {
            let _guard = env_guard();
            let (fd, _parent) = child_side();
            // SAFETY: `F_SETFD` on a descriptor this test owns.
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            let taken = AtomicBool::new(false);
            let lifeline = read_spawner_with(&fd.to_string(), &taken).unwrap().unwrap();
            assert_eq!(lifeline.fd.as_raw_fd(), fd);
            assert_ne!(fd_flags(fd) & libc::FD_CLOEXEC, 0);

            let again = read_spawner_with(&fd.to_string(), &taken)
                .unwrap_err()
                .to_string();
            assert!(again.contains(SPAWNER_LIFELINE_ENV), "{again}");
            assert!(again.contains("already read"), "{again}");
        }

        /// The two reads are guarded separately: taking the spawner lifeline leaves the formation
        /// lifeline readable, and the other way round.
        #[test]
        fn spawner_and_formation_lifelines_do_not_share_a_once_flag() {
            assert!(!std::ptr::eq(&SPAWNER_TAKEN, &FORMATION_TAKEN));
            let _guard = env_guard();
            let (spawner_fd, _parent) = child_side();
            let (formation_fd, _launcher) = member_side();
            std::env::set_var(SPAWNER_LIFELINE_ENV, spawner_fd.to_string());
            std::env::set_var(FORMATION_LIFELINE_ENV, formation_fd.to_string());
            let spawner_taken = AtomicBool::new(false);
            let formation_taken = AtomicBool::new(false);
            let spawner = SpawnerLifeline::from_env_once(&spawner_taken);
            let formation = FormationLifeline::from_env_once(&formation_taken);
            std::env::remove_var(SPAWNER_LIFELINE_ENV);
            std::env::remove_var(FORMATION_LIFELINE_ENV);
            assert_eq!(spawner.unwrap().unwrap().fd.as_raw_fd(), spawner_fd);
            assert_eq!(formation.unwrap().unwrap().fd.as_raw_fd(), formation_fd);
        }

        #[test]
        fn lifeline_absent_or_blank_is_none() {
            let _guard = env_guard();
            std::env::remove_var(FORMATION_LIFELINE_ENV);
            std::env::remove_var(SPAWNER_LIFELINE_ENV);
            let taken = AtomicBool::new(false);
            assert!(FormationLifeline::from_env_once(&taken).unwrap().is_none());
            assert!(read_with("  ", &taken).unwrap().is_none());
            assert!(SpawnerLifeline::from_env_once(&taken).unwrap().is_none());
            assert!(read_spawner_with("  ", &taken).unwrap().is_none());
            assert!(
                !taken.load(Ordering::SeqCst),
                "an absent lifeline takes nothing"
            );
        }

        /// Values that are not a read end, each with the words its refusal must carry, and the
        /// descriptors they name, which stay open while the caller holds them.
        #[allow(clippy::type_complexity)]
        fn refusals() -> (
            Vec<(String, &'static str)>,
            (std::fs::File, std::fs::File, ChildLifeline),
        ) {
            let file = tempfile::tempfile().unwrap();
            let null = std::fs::File::open("/dev/null").unwrap();
            let launcher = ChildLifeline::new().unwrap();
            let write = launcher.write_fd().unwrap();
            let cases = vec![
                ("three".to_string(), "not a decimal descriptor number"),
                ("-4".to_string(), "not a decimal descriptor number"),
                ("2".to_string(), "standard stream"),
                (closed_fd().to_string(), "is not open"),
                (null.as_raw_fd().to_string(), "a character device"),
                (file.as_raw_fd().to_string(), "a regular file"),
                (write.to_string(), "open for writing"),
            ];
            (cases, (file, null, launcher))
        }

        #[test]
        fn lifeline_values_that_are_not_a_read_end_are_refused() {
            let _guard = env_guard();
            let (cases, _held) = refusals();
            for (value, expected) in cases {
                let taken = AtomicBool::new(false);
                let refused = read_with(&value, &taken).unwrap_err().to_string();
                assert!(refused.contains(FORMATION_LIFELINE_ENV), "{refused}");
                assert!(refused.contains(expected), "{value}: {refused}");
                if expected == "open for writing" {
                    assert!(refused.contains("a member holds only"), "{refused}");
                }
            }
        }

        #[test]
        fn spawner_lifeline_values_that_are_not_a_read_end_are_refused() {
            let _guard = env_guard();
            let (cases, _held) = refusals();
            for (value, expected) in cases {
                let taken = AtomicBool::new(false);
                let refused = read_spawner_with(&value, &taken).unwrap_err();
                assert!(
                    matches!(refused, RuntimeError::SpawnerLifelineUnreadable { .. }),
                    "{refused:?}"
                );
                let refused = refused.to_string();
                assert!(refused.contains(SPAWNER_LIFELINE_ENV), "{refused}");
                assert!(refused.contains(expected), "{value}: {refused}");
                if expected == "open for writing" {
                    assert!(refused.contains("a child holds only"), "{refused}");
                }
            }
        }

        /// What `sh` reports about the descriptors it inherited: the variable's value, then
        /// `read` and `write` for whichever end it holds.
        fn handed(variable: &str, read: RawFd, write: RawFd) -> std::process::Command {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(format!(
                "echo \"${variable}\"; \
                 if [ -e /dev/fd/{read} ]; then echo read; fi; \
                 if [ -e /dev/fd/{write} ]; then echo write; fi"
            ));
            command.stdout(std::process::Stdio::piped());
            command
        }

        #[test]
        fn lifeline_hand_to_gives_a_child_its_read_end_and_no_write_end() {
            let mut lifeline = MemberLifeline::new().unwrap();
            let (read, write) = (
                lifeline.ends.read.as_ref().unwrap().as_raw_fd(),
                lifeline.write_fd().unwrap(),
            );
            let mut command = handed(FORMATION_LIFELINE_ENV, read, write);
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
                lifeline.ends.env_pair(FORMATION_LIFELINE_ENV).is_none(),
                "the launcher released its read end"
            );
            assert!(lifeline.write_fd().is_some());
        }

        #[test]
        fn spawner_lifeline_hand_to_gives_a_child_its_read_end_and_no_write_end() {
            let mut lifeline = ChildLifeline::new().unwrap();
            let (read, write) = (
                lifeline.ends.read.as_ref().unwrap().as_raw_fd(),
                lifeline.write_fd().unwrap(),
            );
            assert_eq!(
                lifeline.env_pair(),
                Some((SPAWNER_LIFELINE_ENV, read.to_string()))
            );
            let mut command = handed(SPAWNER_LIFELINE_ENV, read, write);
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
                "the parent released its read end"
            );
            assert!(lifeline.write_fd().is_some());
        }

        /// Watch `fd` as `kind` would, and report each call of `on_closed` on the channel.
        fn watched(kind: &'static Kind, fd: RawFd) -> mpsc::Receiver<()> {
            // SAFETY: `fd` is a read end the calling test released and owns nothing else of.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let (tx, rx) = mpsc::channel();
            kind.watch(fd, move || tx.send(()).unwrap()).unwrap();
            rx
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
        fn spawner_lifeline_closing_the_write_end_fires_the_watcher_once() {
            let _guard = env_guard();
            let (fd, mut parent) = child_side();
            let taken = AtomicBool::new(false);
            let lifeline = read_spawner_with(&fd.to_string(), &taken).unwrap().unwrap();
            let (tx, rx) = mpsc::channel();
            lifeline.watch(move || tx.send(()).unwrap()).unwrap();
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            parent.close();
            parent.close();
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

        /// Dropping the parent's side is a close: a parent that is gone has closed every child's
        /// lifeline.
        #[test]
        fn spawner_lifeline_dropping_the_parent_side_fires_the_watcher() {
            let (fd, parent) = child_side();
            let rx = watched(&SPAWNER, fd);
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            drop(parent);
            rx.recv_timeout(Duration::from_secs(5))
                .expect("dropping the write end fires the watcher");
        }

        /// A write end held until exit stays open after its `ChildLifeline` is gone.
        #[test]
        fn spawner_lifeline_held_until_exit_stays_open() {
            let (fd, parent) = child_side();
            let write = parent.write_fd().unwrap();
            let rx = watched(&SPAWNER, fd);
            parent.hold_until_exit();
            assert!(
                rx.recv_timeout(Duration::from_millis(500)).is_err(),
                "a held write end is not EOF"
            );
            assert!(fd_flags(write) >= 0, "the write end is still open");
            // This test's own stand-in for the process exiting.
            // SAFETY: `write` was leaked by `hold_until_exit` and is owned by nothing.
            drop(unsafe { OwnedFd::from_raw_fd(write) });
            rx.recv_timeout(Duration::from_secs(5))
                .expect("closing the held write end fires the watcher");
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
        fn spawner_lifeline_eof_before_watch_fires_at_once() {
            let (fd, mut parent) = child_side();
            parent.close();
            let rx = watched(&SPAWNER, fd);
            rx.recv_timeout(Duration::from_secs(5))
                .expect("sticky EOF fires the watcher");
        }

        fn bytes_do_not_fire(kind: &'static Kind, fd: RawFd, write: RawFd) -> mpsc::Receiver<()> {
            let rx = watched(kind, fd);
            let bytes = [7u8; 64];
            // SAFETY: writes from a live 64-byte array to a write end the caller holds.
            let written =
                unsafe { libc::write(write, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
            assert_eq!(written, 64);
            assert!(
                rx.recv_timeout(Duration::from_millis(500)).is_err(),
                "bytes are not EOF"
            );
            rx
        }

        #[test]
        fn lifeline_bytes_with_the_write_end_open_do_not_fire_the_watcher() {
            let (fd, mut launcher) = member_side();
            let rx = bytes_do_not_fire(&FORMATION, fd, launcher.write_fd().unwrap());
            launcher.close();
            rx.recv_timeout(Duration::from_secs(5))
                .expect("EOF after the bytes fires");
        }

        #[test]
        fn spawner_lifeline_bytes_with_the_write_end_open_do_not_fire_the_watcher() {
            let (fd, mut parent) = child_side();
            let rx = bytes_do_not_fire(&SPAWNER, fd, parent.write_fd().unwrap());
            parent.close();
            rx.recv_timeout(Duration::from_secs(5))
                .expect("EOF after the bytes fires");
        }

        #[test]
        fn the_watcher_threads_are_named_for_their_lifeline() {
            assert_eq!(FORMATION.thread, "formation-lifeline");
            assert_eq!(SPAWNER.thread, "spawner-lifeline");
        }
    }
}
