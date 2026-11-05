//! The one place the host's namespaces are probed.
//!
//! Every decision that depends on what namespaces this host can give a capsule reads a
//! [`NamespaceReading`]: the `E-CAP-005` network-namespace check, whether a command's literal
//! `/tmp` can be mapped onto the session's temporary folder, whether `sealed` is refused
//! (`E-CAP-003`), the enforcement tier, `mur run --explain-scope` and `mur doctor`. A reading is
//! taken once per [`crate::sandbox::HostProbe`], so one staged session judges one reading.
//!
//! The probe tries, in order, exactly what a session does:
//!
//!   1. **network** — the session's own creator ([`crate::network_namespace`]):
//!      `unshare(CLONE_NEWUSER | CLONE_NEWNET)`, identity maps, `lo`, the local route, the
//!      sockets and the hand-off.
//!   2. **mount** — in a child that joins that namespace the way every command does
//!      (`join_session_netns`), the private mount namespace a command's `/tmp` mapping builds:
//!      `unshare(CLONE_NEWNS)`, `MS_REC | MS_PRIVATE`, a recursive bind over `/tmp`.
//!   3. **sealed root** — in a second joining child, the composed root's nested construction:
//!      `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, identity maps, `MS_REC | MS_PRIVATE`, a `tmpfs`,
//!      `pivot_root(2)`, detaching the old root.
//!   4. **private proc** — in a third joining child that also enters the session's PID namespace
//!      and forks into it, as a command's relay does: `unshare(CLONE_NEWNS)`, `MS_REC |
//!      MS_PRIVATE`, and `mount -t proc` over `/proc` with the `hidepid` spellings
//!      [`crate::proc_view`] mounts with.
//!
//! The creator the network attempt runs also creates the session's PID namespace and its holder,
//! so that attempt exercises the PID namespace too; dropping its handles ends the holder.
//!
//! The mount attempts run inside the probe's network namespace because no command runs outside
//! one: a command joins its session's namespaces first, on every tier, and only then builds its
//! mount namespace. A host that cannot create the network namespace attempts none of them.
//!
//! AppArmor's unprivileged-userns restriction and the profile confining `mur` are read with the
//! attempts, and only ever choose the words of a refusal: no decision reads them.

#[cfg(any(target_os = "linux", test))]
use crate::network_namespace::{decode_step_record, STEP_RECORD_LEN};
use crate::network_namespace::{
    errno_label, errno_words, EgressNamespaceFailure, NamespaceAttemptError,
};
use crate::sealed::{ProfileAttachment, UsernsGrant};

/// One step of a mount rehearsal, as the child reports it when that step fails.
///
/// `JoinSession` is shared by both rehearsals; `UnshareMount`, `MakePrivate` and `BindTmp` are the
/// `/tmp` mapping's; the rest are the composed root's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountStep {
    JoinSession = 1,
    UnshareMount = 2,
    MakePrivate = 3,
    BindTmp = 4,
    UnshareUserMount = 5,
    IdentityMaps = 6,
    MakePrivateNested = 7,
    Tmpfs = 8,
    PivotPrepare = 9,
    PivotRoot = 10,
    DetachOldRoot = 11,
    /// `setns(2)` into the session's PID namespace.
    JoinPidNamespace = 12,
    /// Forking into the session's PID namespace.
    ForkIntoPidNamespace = 13,
    /// `mount -t proc` over `/proc`, every `hidepid` spelling refused.
    MountProc = 14,
}

impl MountStep {
    // Only the Linux probe decodes a step; tests on every OS check the encoding round-trips.
    #[cfg(any(target_os = "linux", test))]
    const ALL: [MountStep; 14] = [
        Self::JoinSession,
        Self::UnshareMount,
        Self::MakePrivate,
        Self::BindTmp,
        Self::UnshareUserMount,
        Self::IdentityMaps,
        Self::MakePrivateNested,
        Self::Tmpfs,
        Self::PivotPrepare,
        Self::PivotRoot,
        Self::DetachOldRoot,
        Self::JoinPidNamespace,
        Self::ForkIntoPidNamespace,
        Self::MountProc,
    ];

    /// The step a rehearsal record's code names, or `None` for a code no step carries.
    #[cfg(any(target_os = "linux", test))]
    fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|step| *step as u32 == code)
    }

    /// The call or operation, as a refusal names it.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::JoinSession => "joining the session's user and network namespaces (setns)",
            Self::UnshareMount => "unshare(CLONE_NEWNS)",
            Self::MakePrivate => "making / MS_REC|MS_PRIVATE",
            Self::BindTmp => "bind-mounting over /tmp",
            Self::UnshareUserMount => {
                "unshare(CLONE_NEWUSER | CLONE_NEWNS) inside the session's user namespace"
            }
            Self::IdentityMaps => "writing uid_map/gid_map",
            Self::MakePrivateNested => "making / MS_REC|MS_PRIVATE in the nested namespace",
            Self::Tmpfs => "mounting the composed root's tmpfs",
            Self::PivotPrepare => "creating the old root's parking directory and entering the root",
            Self::PivotRoot => "pivot_root(2)",
            Self::DetachOldRoot => "detaching the old root",
            Self::JoinPidNamespace => "joining the session's PID namespace (setns)",
            Self::ForkIntoPidNamespace => "forking into the session's PID namespace",
            Self::MountProc => "mounting proc over /proc",
        }
    }
}

/// Why a mount rehearsal produced no namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountAttemptError {
    /// The rehearsal ran, and `step` failed with `errno`.
    Step { step: MountStep, errno: i32 },
    /// The rehearsal child could not be forked, piped or waited for, or exited without reporting;
    /// the runtime's own account of it.
    Rehearsal(String),
}

/// What one mount rehearsal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountAttempt {
    /// Every step succeeded.
    Created,
    /// A step failed, or the rehearsal could not run.
    Failed(MountAttemptError),
    /// Not tried: the network namespace it nests in could not be created, or the host is not
    /// Linux.
    NotAttempted,
}

impl MountAttempt {
    /// `<call> failed with <ERRNO>`, the rehearsal's own account, or `None` when nothing failed:
    /// the one-line answer `mur doctor` prints.
    #[must_use]
    pub fn failure_summary(&self) -> Option<String> {
        match self {
            MountAttempt::Failed(MountAttemptError::Step { step, errno }) => Some(format!(
                "{} failed with {}",
                step.describe(),
                errno_label(*errno)
            )),
            MountAttempt::Failed(MountAttemptError::Rehearsal(detail)) => Some(detail.clone()),
            MountAttempt::Created | MountAttempt::NotAttempted => None,
        }
    }
}

/// The host's namespaces as one probe found them: the three attempts, in the order a session
/// makes them, and the two readings a refusal is worded from.
///
/// Off Linux nothing is attempted and nothing fails: `network` is `Ok(())` because no attempt
/// failed — there is no network namespace to create — and both mount attempts are
/// [`MountAttempt::NotAttempted`]. Every question asked of the reading therefore answers that no
/// namespace was created and none was refused: [`Self::mount_namespace_created`] and
/// [`Self::sealed_root_created`] are `false` and [`Self::egress_namespace_failure`] is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceReading {
    /// The session's network namespace creation.
    pub network: Result<(), NamespaceAttemptError>,
    /// The private mount namespace a command's `/tmp` mapping builds, inside the network
    /// namespace.
    pub mount: MountAttempt,
    /// The composed root's nested user+mount namespace and `pivot_root(2)`, inside the network
    /// namespace.
    pub sealed_root: MountAttempt,
    /// A private `procfs` mounted over `/proc` by a process forked into the session's PID
    /// namespace, in its own mount namespace: what decides [`crate::proc_view::proc_view`].
    pub private_proc: MountAttempt,
    /// AppArmor's unprivileged-userns restriction as read with the attempts. Words a refusal;
    /// decides nothing.
    pub userns_grant: UsernsGrant,
    /// The profile the kernel reports as confining this process, read with the attempts. Words a
    /// refusal; decides nothing.
    pub attachment: ProfileAttachment,
}

impl NamespaceReading {
    /// The reading of a host that is not Linux. See the type's documentation.
    #[must_use]
    pub fn not_linux() -> Self {
        Self {
            network: Ok(()),
            mount: MountAttempt::NotAttempted,
            sealed_root: MountAttempt::NotAttempted,
            private_proc: MountAttempt::NotAttempted,
            userns_grant: UsernsGrant::default(),
            attachment: ProfileAttachment::NotReported,
        }
    }

    /// The `E-CAP-005` failure the network attempt is, or `None` when it created the namespace
    /// (or, off Linux, when there was none to create).
    #[must_use]
    pub fn egress_namespace_failure(&self) -> Option<EgressNamespaceFailure> {
        crate::network_namespace::egress_namespace_failure(
            cfg!(target_os = "linux"),
            self.network.clone(),
            self.userns_grant,
            self.attachment.clone(),
        )
    }

    /// Whether a command can get a private mount namespace inside the session's user namespace,
    /// which is what maps its literal `/tmp`.
    #[must_use]
    pub fn mount_namespace_created(&self) -> bool {
        self.network.is_ok() && self.mount == MountAttempt::Created
    }

    /// Whether a command can mount a private `procfs` of the session's PID namespace over
    /// `/proc`.
    #[must_use]
    pub fn private_proc_created(&self) -> bool {
        self.network.is_ok() && self.private_proc == MountAttempt::Created
    }

    /// Whether the composed root `sealed` runs in was built: the nested user+mount namespace,
    /// its mounts and `pivot_root(2)`.
    #[must_use]
    pub fn sealed_root_created(&self) -> bool {
        self.network.is_ok() && self.sealed_root == MountAttempt::Created
    }

    /// Whether a user+mount namespace, its identity maps and private propagation can be created
    /// inside the session's namespace — the sealed-root rehearsal got past
    /// [`MountStep::MakePrivateNested`]. What a test that builds a bind mount in its own
    /// user+mount namespace needs to ask.
    #[must_use]
    pub fn user_mount_namespace_usable(&self) -> bool {
        self.network.is_ok()
            && match &self.sealed_root {
                MountAttempt::Created => true,
                MountAttempt::Failed(MountAttemptError::Step { step, .. }) => matches!(
                    step,
                    MountStep::Tmpfs
                        | MountStep::PivotPrepare
                        | MountStep::PivotRoot
                        | MountStep::DetachOldRoot
                ),
                MountAttempt::Failed(MountAttemptError::Rehearsal(_))
                | MountAttempt::NotAttempted => false,
            }
    }

    /// The first attempt that failed, as `<call> failed with <ERRNO>` or the runtime's account,
    /// or `None` when none did.
    #[must_use]
    pub fn first_failure(&self) -> Option<String> {
        if let Some(failure) = self.egress_namespace_failure() {
            return Some(failure.summary());
        }
        self.sealed_root
            .failure_summary()
            .or_else(|| self.mount.failure_summary())
    }
}

/// `The probe's <call> failed with <ERRNO> (<words>).`, the sentence an `E-CAP-003` for `sealed`
/// names the failed attempt with.
pub(crate) fn probe_failure_sentence(failed_call: &str, errno: i32) -> String {
    format!(
        "The probe's {failed_call} failed with {} ({}).",
        errno_label(errno),
        errno_words(errno)
    )
}

/// Reads the host's namespaces: the three attempts, in order, and the readings.
///
/// Every `unshare` and `setns` happens in a child, so the calling process is unchanged, and the
/// namespace handles are closed before this returns.
#[cfg(target_os = "linux")]
pub(crate) fn probe_namespaces() -> NamespaceReading {
    use crate::network_namespace::{create_session_netns, CapsuleNetnsPlan};

    let (network, mount, sealed_root, private_proc) =
        match create_session_netns(&CapsuleNetnsPlan::resolve(Vec::new())) {
            Err(error) => (
                Err(error),
                MountAttempt::NotAttempted,
                MountAttempt::NotAttempted,
                MountAttempt::NotAttempted,
            ),
            Ok(created) => {
                let (user_fd, net_fd) = created.namespace_handles();
                let pid_fd = created.pid_namespace_handle();
                let handles = linux::Handles {
                    user_fd,
                    net_fd,
                    pid_fd,
                };
                let mount = linux::rehearse(linux::Rehearsal::PrivateTmp, handles);
                let sealed_root = linux::rehearse(linux::Rehearsal::SealedRoot, handles);
                let private_proc = linux::rehearse(linux::Rehearsal::PrivateProc, handles);
                // Closes the holder's lifeline too, which ends the probe's PID namespace.
                drop(created);
                (Ok(()), mount, sealed_root, private_proc)
            }
        };
    NamespaceReading {
        network,
        mount,
        sealed_root,
        private_proc,
        userns_grant: crate::sealed::userns_grant(),
        attachment: crate::sealed::inspect_profile_attachment(),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn probe_namespaces() -> NamespaceReading {
    NamespaceReading::not_linux()
}

/// What a rehearsal's step report says: `Created` for code `0`, the failed step otherwise, or
/// `None` for a code no step carries.
#[cfg(any(target_os = "linux", test))]
fn decode_record(record: &[u8; STEP_RECORD_LEN]) -> Option<MountAttempt> {
    let (code, errno) = decode_step_record(record);
    if code == 0 {
        return Some(MountAttempt::Created);
    }
    MountStep::from_code(code)
        .map(|step| MountAttempt::Failed(MountAttemptError::Step { step, errno }))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    use super::{decode_record, MountAttempt, MountAttemptError, MountStep, STEP_RECORD_LEN};
    use crate::network_namespace::encode_step_record;
    use crate::sealed::{
        make_dumpable_for_map_writes, restore_dumpable, write_decimal_map, OLD_ROOT_NAME_C,
        OLD_ROOT_PIVOTED_C, SEALED_ROOT_BASE_CANDIDATES,
    };

    /// Which of the three mount rehearsals to run.
    #[derive(Debug, Clone, Copy)]
    pub(super) enum Rehearsal {
        /// A command's `/tmp` mapping: `unshare(CLONE_NEWNS)`, private propagation, a bind over
        /// `/tmp`.
        PrivateTmp,
        /// The composed root's nested namespace, maps, mounts and pivot.
        SealedRoot,
        /// A command's private `/proc`: `setns` into the PID namespace, a fork into it, then in
        /// the forked process `unshare(CLONE_NEWNS)`, private propagation and `mount -t proc`.
        PrivateProc,
    }

    impl Rehearsal {
        fn name(self) -> &'static str {
            match self {
                Rehearsal::PrivateTmp => "mount rehearsal",
                Rehearsal::SealedRoot => "sealed-root rehearsal",
                Rehearsal::PrivateProc => "private-procfs rehearsal",
            }
        }
    }

    /// The session namespace handles a rehearsal joins, held open by the caller.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct Handles {
        pub(super) user_fd: RawFd,
        pub(super) net_fd: RawFd,
        pub(super) pid_fd: RawFd,
    }

    /// Every path the child uses, built in the parent before `fork()`.
    struct Paths {
        /// `/tmp`, canonicalised.
        tmp: CString,
        /// Each root base candidate with its parking directory's full path.
        bases: Vec<(CString, CString)>,
        uid: u32,
        gid: u32,
    }

    impl Paths {
        fn resolve() -> Result<Self, String> {
            let tmp = std::fs::canonicalize("/tmp").unwrap_or_else(|_| "/tmp".into());
            let tmp = CString::new(tmp.as_os_str().as_bytes())
                .map_err(|_| "the canonical /tmp contains a NUL byte".to_string())?;
            let mut bases = Vec::with_capacity(SEALED_ROOT_BASE_CANDIDATES.len());
            for base in SEALED_ROOT_BASE_CANDIDATES {
                let parked = format!(
                    "{base}/{}",
                    OLD_ROOT_NAME_C.to_str().expect("the parking name is ASCII")
                );
                bases.push((
                    CString::new(*base).expect("candidates carry no NUL"),
                    CString::new(parked).expect("candidates carry no NUL"),
                ));
            }
            // SAFETY: `getuid`/`getgid` take no arguments, dereference nothing and cannot fail.
            // Read here, before the fork: inside a fresh user namespace with no map written yet
            // they report the overflow id, which no `uid_map` accepts.
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            Ok(Self {
                tmp,
                bases,
                uid,
                gid,
            })
        }
    }

    /// Runs `rehearsal` in a child that first joins the user and network namespaces `handles`
    /// name, and reports what it did.
    pub(super) fn rehearse(rehearsal: Rehearsal, handles: Handles) -> MountAttempt {
        let failed = |detail: String| MountAttempt::Failed(MountAttemptError::Rehearsal(detail));
        let paths = match Paths::resolve() {
            Ok(paths) => paths,
            Err(detail) => {
                return failed(format!("the {} could not run: {detail}", rehearsal.name()))
            }
        };

        let mut pipe = [0i32; 2];
        // SAFETY: `pipe` is a 2-element stack array, exactly the size `pipe2` writes into.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return failed(format!(
                "pipe2() for the {} failed: {}",
                rehearsal.name(),
                io::Error::last_os_error()
            ));
        }
        // SAFETY: both fds were just returned by the successful `pipe2` and nothing else owns
        // them.
        let (read_end, write_end) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        let write_fd = write_end.as_raw_fd();

        // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the child
        // touches nothing but async-signal-safe primitives. The child runs only `setns`,
        // `unshare`, `prctl`, `mount`, `open`/`write`/`close`, `mkdir`, `chdir`, `pivot_root`
        // via `syscall`, `umount2`, `fork` and `waitpid` over paths built above, then `write`
        // and `_exit`: no allocation, no locks, no stdio.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return failed(format!(
                "fork() for the {} failed: {}",
                rehearsal.name(),
                io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            // SAFETY: forked-child context, syscalls only; the block ends in `_exit`, so no
            // destructor of the parent's state runs here.
            unsafe {
                let record = match run(rehearsal, &paths, handles, write_fd) {
                    Ok(()) => encode_step_record(0, 0),
                    Err((step, errno)) => encode_step_record(step as u32, errno),
                };
                libc::write(write_fd, record.as_ptr().cast(), record.len());
                libc::_exit(0);
            }
        }
        drop(write_end);

        let status = match crate::network_namespace::reap_child(pid) {
            Ok(status) => status,
            Err(error) => {
                return failed(format!(
                    "waiting for the {} failed: {error}",
                    rehearsal.name()
                ))
            }
        };
        // Read after the child is reaped and without blocking: the record it wrote stays in the
        // pipe, and another thread's child forked in the meantime may still hold the write end.
        let mut record = [0u8; STEP_RECORD_LEN];
        // SAFETY: `record` is a live stack buffer of `STEP_RECORD_LEN` bytes.
        let read = unsafe {
            libc::read(
                read_end.as_raw_fd(),
                record.as_mut_ptr().cast(),
                STEP_RECORD_LEN,
            )
        };
        if read == STEP_RECORD_LEN as isize {
            if let Some(attempt) = decode_record(&record) {
                return attempt;
            }
        }
        failed(format!(
            "the {} exited without reporting ({status})",
            rehearsal.name()
        ))
    }

    /// The rehearsal's steps, each failure the step and the `errno` it left.
    ///
    /// # Safety
    /// Post-`fork()` child context only. Every call is async-signal-safe and nothing here
    /// allocates: every path is in `paths`, built before the fork.
    unsafe fn run(
        rehearsal: Rehearsal,
        paths: &Paths,
        handles: Handles,
        report_fd: RawFd,
    ) -> Result<(), (MountStep, i32)> {
        // SAFETY: the caller's contract covers this whole body.
        unsafe {
            let errno = || *libc::__errno_location();
            // Every command joins its session's namespaces before anything else, on every tier.
            if let Err(error) =
                crate::network_namespace::join_session_netns(handles.user_fd, handles.net_fd)
            {
                return Err((MountStep::JoinSession, error.raw_os_error().unwrap_or(0)));
            }
            match rehearsal {
                // A command's relay and command: `setns` into the PID namespace, then a fork, so
                // the mounting process is a member of the namespace its `procfs` describes.
                Rehearsal::PrivateProc => {
                    if libc::setns(handles.pid_fd, libc::CLONE_NEWPID) != 0 {
                        return Err((MountStep::JoinPidNamespace, errno()));
                    }
                    let member = libc::fork();
                    if member < 0 {
                        return Err((MountStep::ForkIntoPidNamespace, errno()));
                    }
                    if member == 0 {
                        // The member reports for the whole rehearsal, then exits `0`.
                        let record = match crate::proc_view::mount_private_proc(true) {
                            Ok(_) => encode_step_record(0, 0),
                            Err((step, step_errno)) => {
                                let step = match step {
                                    crate::proc_view::ProcMountStep::UnshareMount => {
                                        MountStep::UnshareMount
                                    }
                                    crate::proc_view::ProcMountStep::MakePrivate => {
                                        MountStep::MakePrivate
                                    }
                                    crate::proc_view::ProcMountStep::MountProc
                                    | crate::proc_view::ProcMountStep::OpenProc => {
                                        MountStep::MountProc
                                    }
                                };
                                encode_step_record(step as u32, step_errno)
                            }
                        };
                        libc::write(report_fd, record.as_ptr().cast(), record.len());
                        libc::_exit(0);
                    }
                    let mut status = 0;
                    loop {
                        let waited = libc::waitpid(member, &mut status, 0);
                        if waited == member {
                            break;
                        }
                        if waited < 0 && errno() == libc::EINTR {
                            continue;
                        }
                        return Err((MountStep::ForkIntoPidNamespace, errno()));
                    }
                    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                        // The member wrote the rehearsal's record; this child adds none.
                        libc::_exit(0);
                    }
                    return Err((MountStep::ForkIntoPidNamespace, 0));
                }
                // `temp_dir::map_private_tmp`'s namespace and propagation, then a bind over `/tmp`
                // with its flags: recursive, because a mount namespace owned by a less privileged
                // user namespace refuses a plain bind of a tree with mounts beneath it (`EINVAL`).
                Rehearsal::PrivateTmp => {
                    if libc::unshare(libc::CLONE_NEWNS) != 0 {
                        return Err((MountStep::UnshareMount, errno()));
                    }
                    if make_private() != 0 {
                        return Err((MountStep::MakePrivate, errno()));
                    }
                    if libc::mount(
                        paths.tmp.as_ptr(),
                        paths.tmp.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err((MountStep::BindTmp, errno()));
                    }
                }
                // `sealed::construct_composed_root`'s steps 1–4, 6 and 7, with the same syscalls
                // in the same order.
                Rehearsal::SealedRoot => {
                    if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                        return Err((MountStep::UnshareUserMount, errno()));
                    }
                    // Dumpable for the map writes only: a non-dumpable task's `/proc/self/*`
                    // entries are root-owned (see `sealed::make_dumpable_for_map_writes`).
                    let previous = make_dumpable_for_map_writes();
                    let _ = write_decimal_map(c"/proc/self/setgroups", None, 0);
                    let mapped =
                        write_decimal_map(c"/proc/self/uid_map", Some(paths.uid), paths.uid)
                            .is_ok()
                            && write_decimal_map(c"/proc/self/gid_map", Some(paths.gid), paths.gid)
                                .is_ok();
                    let map_errno = errno();
                    restore_dumpable(previous);
                    if !mapped {
                        return Err((MountStep::IdentityMaps, map_errno));
                    }
                    if make_private() != 0 {
                        return Err((MountStep::MakePrivateNested, errno()));
                    }
                    // The first candidate that takes a `tmpfs`; the real construction picks one
                    // that is no ancestor of the workdir, which a rehearsal has none of.
                    let mut based = None;
                    let mut tmpfs_errno = 0;
                    for (base, parked) in &paths.bases {
                        if libc::mount(
                            c"tmpfs".as_ptr(),
                            base.as_ptr(),
                            c"tmpfs".as_ptr(),
                            libc::MS_NOSUID | libc::MS_NODEV,
                            c"mode=0755".as_ptr().cast(),
                        ) == 0
                        {
                            based = Some((base, parked));
                            break;
                        }
                        tmpfs_errno = errno();
                    }
                    let Some((base, parked)) = based else {
                        return Err((MountStep::Tmpfs, tmpfs_errno));
                    };
                    if libc::mkdir(parked.as_ptr(), 0o700) != 0 {
                        return Err((MountStep::PivotPrepare, errno()));
                    }
                    if libc::chdir(base.as_ptr()) != 0 {
                        return Err((MountStep::PivotPrepare, errno()));
                    }
                    if libc::syscall(
                        libc::SYS_pivot_root,
                        c".".as_ptr(),
                        OLD_ROOT_NAME_C.as_ptr(),
                    ) != 0
                    {
                        return Err((MountStep::PivotRoot, errno()));
                    }
                    if libc::chdir(c"/".as_ptr()) != 0 {
                        return Err((MountStep::PivotRoot, errno()));
                    }
                    if libc::umount2(OLD_ROOT_PIVOTED_C.as_ptr(), libc::MNT_DETACH) != 0 {
                        return Err((MountStep::DetachOldRoot, errno()));
                    }
                }
            }
            Ok(())
        }
    }

    /// `mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL)`.
    ///
    /// # Safety
    /// Post-`fork()` child context only.
    unsafe fn make_private() -> libc::c_int {
        // SAFETY: every pointer is null or a static C string.
        unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_namespace::{encode_step_record, CreationStep};

    fn created() -> NamespaceReading {
        NamespaceReading {
            network: Ok(()),
            mount: MountAttempt::Created,
            sealed_root: MountAttempt::Created,
            private_proc: MountAttempt::NotAttempted,
            userns_grant: UsernsGrant::Withheld,
            attachment: ProfileAttachment::Unconfined,
        }
    }

    fn failed(step: MountStep, errno: i32) -> MountAttempt {
        MountAttempt::Failed(MountAttemptError::Step { step, errno })
    }

    #[test]
    fn every_record_round_trips_and_an_unknown_code_is_none() {
        assert_eq!(
            decode_record(&encode_step_record(0, 0)),
            Some(MountAttempt::Created)
        );
        for step in MountStep::ALL {
            assert_eq!(
                decode_record(&encode_step_record(step as u32, libc::EPERM)),
                Some(failed(step, libc::EPERM))
            );
        }
        assert_eq!(decode_record(&encode_step_record(99, 0)), None);
    }

    #[test]
    fn every_step_names_its_own_call() {
        let mut seen = std::collections::HashSet::new();
        for step in MountStep::ALL {
            assert!(
                seen.insert(step.describe()),
                "{step:?} repeats a description"
            );
        }
        assert_eq!(MountStep::UnshareMount.describe(), "unshare(CLONE_NEWNS)");
        assert_eq!(MountStep::PivotRoot.describe(), "pivot_root(2)");
    }

    #[test]
    fn the_questions_follow_the_attempts_and_never_the_readings() {
        for grant in UsernsGrant::ALL {
            let reading = NamespaceReading {
                userns_grant: *grant,
                ..created()
            };
            assert!(reading.mount_namespace_created());
            assert!(reading.sealed_root_created());
            assert!(reading.user_mount_namespace_usable());
            assert_eq!(reading.egress_namespace_failure(), None);
            assert_eq!(reading.first_failure(), None);
        }
    }

    #[test]
    fn a_network_failure_creates_nothing_after_it() {
        let reading = NamespaceReading {
            network: Err(NamespaceAttemptError::Step {
                step: CreationStep::Unshare,
                errno: libc::EPERM,
            }),
            mount: MountAttempt::NotAttempted,
            sealed_root: MountAttempt::NotAttempted,
            ..created()
        };
        assert!(!reading.mount_namespace_created());
        assert!(!reading.sealed_root_created());
        assert!(!reading.user_mount_namespace_usable());
        assert_eq!(
            reading.first_failure().as_deref(),
            cfg!(target_os = "linux")
                .then_some("unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with EPERM")
        );
    }

    #[test]
    fn the_user_mount_question_needs_the_nested_namespace_its_maps_and_propagation() {
        for (step, usable) in [
            (MountStep::JoinSession, false),
            (MountStep::UnshareUserMount, false),
            (MountStep::IdentityMaps, false),
            (MountStep::MakePrivateNested, false),
            (MountStep::Tmpfs, true),
            (MountStep::PivotPrepare, true),
            (MountStep::PivotRoot, true),
            (MountStep::DetachOldRoot, true),
        ] {
            let reading = NamespaceReading {
                sealed_root: failed(step, libc::EPERM),
                ..created()
            };
            assert_eq!(reading.user_mount_namespace_usable(), usable, "{step:?}");
            assert!(!reading.sealed_root_created());
            assert!(reading.mount_namespace_created());
        }
        let reading = NamespaceReading {
            sealed_root: MountAttempt::Failed(MountAttemptError::Rehearsal("x".to_string())),
            ..created()
        };
        assert!(!reading.user_mount_namespace_usable());
    }

    #[test]
    fn off_linux_nothing_is_created_and_nothing_is_refused() {
        let reading = NamespaceReading::not_linux();
        assert!(!reading.mount_namespace_created());
        assert!(!reading.sealed_root_created());
        assert!(!reading.user_mount_namespace_usable());
        assert!(reading.mount.failure_summary().is_none());
        assert!(reading.sealed_root.failure_summary().is_none());
        #[cfg(not(target_os = "linux"))]
        assert_eq!(probe_namespaces(), NamespaceReading::not_linux());
    }

    #[test]
    fn a_failed_attempt_summarises_as_its_call_and_errno() {
        assert_eq!(
            failed(MountStep::MakePrivate, libc::EACCES)
                .failure_summary()
                .as_deref(),
            Some("making / MS_REC|MS_PRIVATE failed with EACCES")
        );
        assert_eq!(
            probe_failure_sentence(MountStep::PivotRoot.describe(), libc::EPERM),
            "The probe's pivot_root(2) failed with EPERM (Operation not permitted)."
        );
    }

    #[cfg(target_os = "linux")]
    const INNER_RUN_ENV: &str = "MURMUR_NAMESPACE_PROBE_UNCHANGED_INNER";

    /// The probe runs every attempt in a child: the calling process keeps its namespaces, and no
    /// descriptor it opened outlives it.
    ///
    /// `/proc/self/fd` counts every thread's descriptors, so the count is exact only with no
    /// other test running: the check runs in a single-threaded re-exec of this binary.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_leaves_the_calling_process_unchanged() {
        let exe = std::env::current_exe().expect("test binary path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "namespace_probe::tests::inner_the_probe_leaves_the_calling_process_unchanged",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(INNER_RUN_ENV, "1")
            .output()
            .expect("re-exec the test binary for the single-threaded inner run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "inner probe run failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "inner run did not execute the probe test\n--- stdout ---\n{stdout}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "run by the_probe_leaves_the_calling_process_unchanged"]
    fn inner_the_probe_leaves_the_calling_process_unchanged() {
        if std::env::var(INNER_RUN_ENV).is_err() {
            return;
        }
        let namespaces = || {
            ["user", "net", "mnt"].map(|ns| std::fs::read_link(format!("/proc/self/ns/{ns}")).ok())
        };
        let fds = || {
            std::fs::read_dir("/proc/self/fd")
                .map(|dir| dir.count())
                .ok()
        };
        let before = (namespaces(), fds());
        let reading = probe_namespaces();
        assert_eq!((namespaces(), fds()), before, "{reading:?}");
        if reading.network.is_err() {
            assert_eq!(reading.mount, MountAttempt::NotAttempted);
            assert_eq!(reading.sealed_root, MountAttempt::NotAttempted);
        }
    }
}
