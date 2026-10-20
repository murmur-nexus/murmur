//! The session's temporary folder: `tmp/` in the session's records directory, which every shell
//! command, plan shell step and native tool reaches as `$TMPDIR`, `$TMP` and `$TEMP`, and on Linux
//! as a literal `/tmp` wherever the host can give a command a private mount namespace.
//!
//! ```text
//! ~/.murmur/records/<workdir root>/<session-id>/
//! └── tmp/        0700, created at staging, writable by the session's commands
//! ```
//!
//! One folder per session, never inside the workdir and never shared between sessions. Its bytes
//! count against `capabilities.resources.workdir_max_bytes` with the workdir's
//! ([`crate::resources::WorkdirGuard`]), and `trace.retain` removes it with the records directory.
//! The runtime itself writes nothing in it.
//!
//! [`temp_mapping`] is the one decision of whether a literal `/tmp` reaches the folder. Staging
//! makes it once, and the session's [`crate::containment::ScopeReport`] and its enforcement both
//! read that answer; `mur run --explain-scope` and `mur doctor` make it from the same inputs.
//!
//! | Applied tier | Mount namespace usable | Literal `/tmp` | `TMPDIR` |
//! |---|---|---|---|
//! | `KernelSealed` | required by the tier | the composed root binds the folder | `/tmp` |
//! | `KernelFull`, `KernelSeccompOnly` | yes | a private mount namespace per command binds it | `/tmp` |
//! | `KernelFull` | no | not mapped; Landlock refuses it | the folder |
//! | `KernelSeccompOnly` | no | not mapped; the host's `/tmp` | the folder |
//! | `EnvironmentOnly` | — | not mapped; the host's `/tmp` | the folder |
//! | any but `KernelSealed`, workdir is `/tmp` | — | not mapped | the folder |

use std::path::{Path, PathBuf};

use crate::sandbox::EnforcementTier;

/// The folder's name beneath the session's records directory.
pub const SESSION_TEMP_DIR_NAME: &str = "tmp";

/// The path a mapped command reaches the folder at, and the value of `TMPDIR` there.
pub const TMP_PATH: &str = "/tmp";

/// The variables naming the folder in every capsule command's environment. Runtime-owned: no
/// manifest key, override or host value can set them.
pub const TEMP_DIR_ENV: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

/// What every mapping failure's message starts with.
pub const TMP_MAP_FAILURE_PREFIX: &str =
    "sandbox: could not map /tmp onto this session's temporary folder:";

/// How a literal `/tmp` reaches the session's temporary folder, or why it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempMapping {
    /// The sealed composed root binds the folder at `/tmp`.
    ComposedRoot,
    /// Each command enters a private mount namespace in which the folder is bound over `/tmp`.
    MountNamespace,
    /// A literal `/tmp` is not the folder; `TMPDIR` names the folder's absolute path.
    NotMapped(NotMappedReason),
}

/// Why a session's `/tmp` is not mapped onto its temporary folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotMappedReason {
    /// No kernel containment at all: macOS and every non-Linux host.
    NoKernelContainment,
    /// Neither a mount namespace nor Landlock.
    NoMountNamespaceNoLandlock,
    /// Landlock without a usable mount namespace.
    NoMountNamespace,
    /// The accessible workdir is `/tmp` itself, which a bind over `/tmp` would hide.
    WorkdirIsTmp,
}

impl TempMapping {
    /// Whether a literal `/tmp` is the folder.
    #[must_use]
    pub fn is_mapped(self) -> bool {
        !matches!(self, Self::NotMapped(_))
    }

    /// The mechanism's wire name, `null` in the report when not mapped.
    #[must_use]
    pub fn mapped_by(self) -> Option<&'static str> {
        match self {
            Self::ComposedRoot => Some("composed-root"),
            Self::MountNamespace => Some("mount-namespace"),
            Self::NotMapped(_) => None,
        }
    }

    /// The value of `TMPDIR`, `TMP` and `TEMP` for a session whose folder is at `folder`.
    #[must_use]
    pub fn tmpdir_value(self, folder: &Path) -> String {
        if self.is_mapped() {
            TMP_PATH.to_string()
        } else {
            folder.to_string_lossy().into_owned()
        }
    }
}

impl NotMappedReason {
    /// The reason in words an operator can act on: `not_mapped_because` in the report.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::NoKernelContainment => {
                "this host has no kernel containment for capsule commands, so a literal /tmp path \
                 is the host's own /tmp; TMPDIR names the session's temporary folder"
            }
            Self::NoMountNamespaceNoLandlock => {
                "this host can give capsule commands neither a private mount namespace nor \
                 Landlock, so a literal /tmp path is the host's own /tmp; TMPDIR names the \
                 session's temporary folder"
            }
            Self::NoMountNamespace => {
                "this host cannot give capsule commands a private mount namespace, so a literal \
                 /tmp path is refused; TMPDIR names the session's temporary folder"
            }
            Self::WorkdirIsTmp => {
                "the workdir is /tmp itself, which mapping /tmp would hide; TMPDIR names the \
                 session's temporary folder"
            }
        }
    }
}

/// The host's `/tmp`, canonicalized, or `/tmp` itself when it cannot be resolved.
pub(crate) fn canonical_tmp() -> PathBuf {
    std::fs::canonicalize(TMP_PATH).unwrap_or_else(|_| PathBuf::from(TMP_PATH))
}

/// The mapping the tier `applied` gives with no usable mount namespace: the composed root's on
/// `KernelSealed`, and unmapped on every other tier. What an enforcement that was never handed
/// its session's [`temp_mapping`] answer installs.
pub(crate) fn tier_temp_mapping(applied: EnforcementTier) -> TempMapping {
    match applied {
        EnforcementTier::KernelSealed => TempMapping::ComposedRoot,
        EnforcementTier::KernelFull => TempMapping::NotMapped(NotMappedReason::NoMountNamespace),
        EnforcementTier::KernelSeccompOnly => {
            TempMapping::NotMapped(NotMappedReason::NoMountNamespaceNoLandlock)
        }
        EnforcementTier::EnvironmentOnly => {
            TempMapping::NotMapped(NotMappedReason::NoKernelContainment)
        }
    }
}

/// Whether a literal `/tmp` reaches the session's temporary folder on the tier `applied`, given
/// whether the session's [`crate::sandbox::HostProbe`] found a usable mount namespace and the
/// workdir the session's commands run in.
///
/// Pure apart from canonicalizing `accessible_workdir` and `/tmp`; a workdir that does not exist
/// yet is compared as given.
pub(crate) fn temp_mapping(
    applied: EnforcementTier,
    mount_namespace_usable: bool,
    accessible_workdir: &Path,
) -> TempMapping {
    let workdir_is_tmp = || {
        let workdir = std::fs::canonicalize(accessible_workdir)
            .unwrap_or_else(|_| accessible_workdir.to_path_buf());
        workdir == canonical_tmp()
    };
    match applied {
        EnforcementTier::KernelFull | EnforcementTier::KernelSeccompOnly if workdir_is_tmp() => {
            TempMapping::NotMapped(NotMappedReason::WorkdirIsTmp)
        }
        EnforcementTier::KernelFull | EnforcementTier::KernelSeccompOnly
            if mount_namespace_usable =>
        {
            TempMapping::MountNamespace
        }
        _ => tier_temp_mapping(applied),
    }
}

/// The paths beneath the host's `/tmp` a mapped command still needs, each re-exposed at its own
/// path inside the folder bound over `/tmp`, with whether its bind is read-only: `writable` (the
/// workdirs) read-write, `read_only` (the runtime and records directories and the Landlock grant
/// paths) read-only.
///
/// Exact paths, never a whole entry of `/tmp`: Landlock accumulates rights up the path walk,
/// across mount boundaries, so everything bound beneath the folder also holds the folder's
/// rights. A read-write bind is the workdir, whose own rule is that same right-set; every other
/// bind is read-only, so those rights write nothing there. Nothing else of the host's `/tmp` is
/// visible.
///
/// Each path is canonicalized first; one that cannot be, or that does not lie strictly beneath
/// `canonical_tmp`, is skipped. The result is in path order, so a bind is made before any bind
/// beneath it. A path beneath a read-write path is dropped, since that bind already carries it;
/// a read-write path beneath a read-only one is kept and bound over it. A path given both ways is
/// read-write.
#[cfg(target_os = "linux")]
pub(crate) fn reexposed_paths<'a>(
    canonical_tmp: &Path,
    writable: impl IntoIterator<Item = &'a Path>,
    read_only: impl IntoIterator<Item = &'a Path>,
) -> Vec<(PathBuf, bool)> {
    let beneath_tmp = |path: &Path| {
        std::fs::canonicalize(path)
            .ok()
            .filter(|canonical| canonical != canonical_tmp && canonical.starts_with(canonical_tmp))
    };
    let mut wanted: Vec<(PathBuf, bool)> = Vec::new();
    for (path, is_read_only) in writable
        .into_iter()
        .map(|path| (path, false))
        .chain(read_only.into_iter().map(|path| (path, true)))
    {
        let Some(canonical) = beneath_tmp(path) else {
            continue;
        };
        match wanted.iter_mut().find(|(seen, _)| *seen == canonical) {
            Some((_, seen_read_only)) => *seen_read_only &= is_read_only,
            None => wanted.push((canonical, is_read_only)),
        }
    }
    wanted.sort();
    let mut kept: Vec<(PathBuf, bool)> = Vec::new();
    for (path, is_read_only) in wanted {
        let under_writable = kept
            .iter()
            .any(|(outer, outer_read_only)| !outer_read_only && path.starts_with(outer));
        if !under_writable {
            kept.push((path, is_read_only));
        }
    }
    kept
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) use linux::TmpMapFailure;
#[cfg(target_os = "linux")]
pub(crate) use linux::{map_private_tmp, TmpMapSpec};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicI32, Ordering};

    use crate::sealed::{open_verified_dir, proc_fd_path, remount_read_only, PROC_FD_PATH_LEN};

    /// One path beneath the host's `/tmp` a mapped command still needs, re-exposed at its own
    /// path.
    struct Reexposure {
        /// The path on the host, `/tmp/<components>`.
        source: CString,
        /// Its components beneath `/tmp`, the mountpoint's path inside the folder bound over it.
        components: Vec<CString>,
        /// Whether the path is a directory; a mountpoint for anything else is an empty file.
        directory: bool,
        /// Whether the bind is remounted read-only.
        read_only: bool,
        /// The source's `O_PATH` descriptor, taken in the child after `unshare(CLONE_NEWNS)` and
        /// before `/tmp` is covered. `-1` until then.
        fd: AtomicI32,
    }

    /// Everything [`map_private_tmp`] needs, resolved in the parent so the child allocates
    /// nothing.
    pub(crate) struct TmpMapSpec {
        /// The folder's host path, and the `(st_dev, st_ino)` of the parent's descriptor on it,
        /// which the child's open must reach.
        folder: CString,
        folder_display: PathBuf,
        folder_identity: (u64, u64),
        /// Canonical `/tmp`, the bind's target.
        tmp: CString,
        tmp_display: PathBuf,
        reexposures: Vec<Reexposure>,
        /// The command's working directory, re-entered once `/tmp` is covered.
        workdir: CString,
        workdir_display: PathBuf,
    }

    /// The step of [`map_private_tmp`] that failed, and its `errno`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct TmpMapFailure {
        step: Step,
        errno: i32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Unshare,
        MakePrivate,
        OpenSource(usize),
        OpenFolder,
        BindFolder,
        OpenTmp,
        CreateMountpoint(usize),
        OpenMountpoint(usize),
        BindReexposure(usize),
        RemountReadOnly(usize),
        Chdir,
    }

    impl TmpMapFailure {
        fn at(step: Step) -> Self {
            Self {
                step,
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            }
        }

        /// A failure at the first step with `errno`, for the forced-failure test seam.
        #[cfg(test)]
        pub(crate) fn forced(errno: i32) -> Self {
            Self {
                step: Step::Unshare,
                errno,
            }
        }
    }

    impl TmpMapSpec {
        /// The spec for one command: `folder` at `folder_identity`, bound over `canonical_tmp`,
        /// with each of `reexposed` (from [`super::reexposed_paths`]) bound back at its own
        /// path, read-only where it says so, and `workdir` re-entered afterwards.
        pub(crate) fn new(
            folder: &Path,
            folder_identity: (u64, u64),
            canonical_tmp: &Path,
            reexposed: &[(PathBuf, bool)],
            workdir: &Path,
        ) -> Result<Self, String> {
            let c_path = |path: &Path| {
                CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                    format!(
                        "{} {} holds a NUL byte",
                        super::TMP_MAP_FAILURE_PREFIX,
                        path.display()
                    )
                })
            };
            let mut reexposures = Vec::with_capacity(reexposed.len());
            for (source, read_only) in reexposed {
                let Ok(beneath) = source.strip_prefix(canonical_tmp) else {
                    continue;
                };
                let components = beneath
                    .components()
                    .map(|component| c_path(Path::new(component.as_os_str())))
                    .collect::<Result<Vec<_>, _>>()?;
                if components.is_empty() {
                    continue;
                }
                reexposures.push(Reexposure {
                    source: c_path(source)?,
                    components,
                    directory: std::fs::symlink_metadata(source)
                        .map(|meta| meta.is_dir())
                        .unwrap_or(true),
                    read_only: *read_only,
                    fd: AtomicI32::new(-1),
                });
            }
            Ok(Self {
                folder: c_path(folder)?,
                folder_display: folder.to_path_buf(),
                folder_identity,
                tmp: c_path(canonical_tmp)?,
                tmp_display: canonical_tmp.to_path_buf(),
                reexposures,
                workdir: c_path(workdir)?,
                workdir_display: workdir.to_path_buf(),
            })
        }

        /// The host paths re-exposed beneath `/tmp`, in bind order, each with whether it is
        /// read-only.
        #[cfg(test)]
        pub(crate) fn reexposed_sources(&self) -> Vec<(String, bool)> {
            self.reexposures
                .iter()
                .map(|entry| (entry.source.to_string_lossy().into_owned(), entry.read_only))
                .collect()
        }

        /// `failure` as the message that reaches the operator:
        /// `sandbox: could not map /tmp onto this session's temporary folder: <step> failed:
        /// <errno text>`.
        pub(crate) fn describe(&self, failure: TmpMapFailure) -> String {
            let tmp = self.tmp_display.display();
            let entry = |index: usize| {
                self.reexposures
                    .get(index)
                    .map(|entry| {
                        entry
                            .components
                            .iter()
                            .map(|component| component.to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/")
                    })
                    .unwrap_or_default()
            };
            let source = |index: usize| {
                self.reexposures
                    .get(index)
                    .map(|entry| entry.source.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            let step = match failure.step {
                Step::Unshare => "unshare(CLONE_NEWNS)".to_string(),
                Step::MakePrivate => "making / private (MS_REC|MS_PRIVATE)".to_string(),
                Step::OpenSource(index) => format!("opening {} to re-expose it", source(index)),
                Step::OpenFolder => format!(
                    "opening the temporary folder {}",
                    self.folder_display.display()
                ),
                Step::BindFolder => format!(
                    "binding the temporary folder {} over {tmp}",
                    self.folder_display.display()
                ),
                Step::OpenTmp => format!("opening the new {tmp}"),
                Step::CreateMountpoint(index) => {
                    format!("creating the mountpoint {tmp}/{}", entry(index))
                }
                Step::OpenMountpoint(index) => {
                    format!("opening the mountpoint {tmp}/{}", entry(index))
                }
                Step::BindReexposure(index) => {
                    format!("binding {} back at its own path", source(index))
                }
                Step::RemountReadOnly(index) => {
                    format!("remounting {} read-only", source(index))
                }
                Step::Chdir => format!(
                    "entering the working directory {}",
                    self.workdir_display.display()
                ),
            };
            format!(
                "{} {step} failed: {}",
                super::TMP_MAP_FAILURE_PREFIX,
                std::io::Error::from_raw_os_error(failure.errno)
            )
        }
    }

    /// Closes `fd`, leaving `errno` as the last call before it left it.
    ///
    /// # Safety
    /// `fd` is open and owned by the caller. Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn close_keeping_errno(fd: libc::c_int) {
        let errno = *libc::__errno_location();
        libc::close(fd);
        *libc::__errno_location() = errno;
    }

    /// `mount --bind` of the descriptor `source` onto the descriptor `target`, recursively, by
    /// their `/proc/self/fd` paths.
    ///
    /// # Safety
    /// Both descriptors are open. Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn bind_fd_onto_fd(source: libc::c_int, target: libc::c_int) -> Result<(), ()> {
        let mut source_path = [0u8; PROC_FD_PATH_LEN];
        let mut target_path = [0u8; PROC_FD_PATH_LEN];
        proc_fd_path(source, &mut source_path)?;
        proc_fd_path(target, &mut target_path)?;
        if libc::mount(
            source_path.as_ptr() as *const libc::c_char,
            target_path.as_ptr() as *const libc::c_char,
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) != 0
        {
            return Err(());
        }
        Ok(())
    }

    /// Whether the descriptor `fd` is a directory (`directory`) or a regular file.
    ///
    /// # Safety
    /// Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn fd_kind_is(fd: libc::c_int, directory: bool) -> bool {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut stat) != 0 {
            return false;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        if directory {
            kind == libc::S_IFDIR
        } else {
            kind == libc::S_IFREG
        }
    }

    /// Closes every re-exposure source descriptor taken so far.
    ///
    /// # Safety
    /// Only in the process that took them, on [`map_private_tmp`]'s terms.
    unsafe fn close_sources(spec: &TmpMapSpec) {
        for entry in &spec.reexposures {
            let fd = entry.fd.swap(-1, Ordering::Relaxed);
            if fd >= 0 {
                close_keeping_errno(fd);
            }
        }
    }

    /// Gives the calling process a private mount namespace in which `spec`'s folder is bound over
    /// `/tmp`, every re-exposed entry of the host's `/tmp` is bound back at its own path, and the
    /// working directory is re-entered.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec only, with every capability in the caller's user namespace (the
    /// session's, after `join_session_netns`) and before the seccomp filter that denies `unshare`
    /// and `mount`. Allocation-free: every path and buffer was built in the parent.
    pub(crate) unsafe fn map_private_tmp(spec: &TmpMapSpec) -> Result<(), TmpMapFailure> {
        // 1. A mount namespace of the command's own, owned by the session's user namespace.
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(TmpMapFailure::at(Step::Unshare));
        }
        // 2. Nothing mounted from here on propagates anywhere.
        if libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(TmpMapFailure::at(Step::MakePrivate));
        }
        // 3. Every re-exposure source, while `/tmp` still shows the host's tree. Opened in this
        //    namespace, since a bind's source must be a mount in the caller's own.
        for (index, entry) in spec.reexposures.iter().enumerate() {
            let fd = libc::open(
                entry.source.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            );
            if fd < 0 {
                let failure = TmpMapFailure::at(Step::OpenSource(index));
                close_sources(spec);
                return Err(failure);
            }
            entry.fd.store(fd, Ordering::Relaxed);
            if !fd_kind_is(fd, entry.directory) {
                close_sources(spec);
                *libc::__errno_location() = libc::ESTALE;
                return Err(TmpMapFailure::at(Step::OpenSource(index)));
            }
        }
        // 4. The folder, verified against the parent's descriptor, bound over `/tmp`. A link
        //    swapped onto its path fails here with `ESTALE` rather than redirecting the bind.
        let folder = match open_verified_dir(&spec.folder, spec.folder_identity) {
            Ok(fd) => fd,
            Err(()) => {
                let failure = TmpMapFailure::at(Step::OpenFolder);
                close_sources(spec);
                return Err(failure);
            }
        };
        let tmp_target = libc::open(
            spec.tmp.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        let bound = tmp_target >= 0 && bind_fd_onto_fd(folder, tmp_target).is_ok();
        let failure = (!bound).then(|| TmpMapFailure::at(Step::BindFolder));
        close_keeping_errno(folder);
        if tmp_target >= 0 {
            close_keeping_errno(tmp_target);
        }
        if let Some(failure) = failure {
            close_sources(spec);
            return Err(failure);
        }
        // 5. Each re-exposure, in path order: its mountpoint inside the new `/tmp`, every
        //    directory to it created or opened with no link followed, the source bound onto it,
        //    and a read-only one remounted so.
        let new_tmp = libc::open(
            spec.tmp.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        );
        if new_tmp < 0 {
            let failure = TmpMapFailure::at(Step::OpenTmp);
            close_sources(spec);
            return Err(failure);
        }
        let result = reexpose_all(spec, new_tmp);
        close_keeping_errno(new_tmp);
        close_sources(spec);
        result?;
        // 6. The working directory std entered before this closure ran lies on the covered
        //    `/tmp` when the workdir is beneath it; entering it again by path lands on the
        //    visible mount.
        if libc::chdir(spec.workdir.as_ptr()) != 0 {
            return Err(TmpMapFailure::at(Step::Chdir));
        }
        Ok(())
    }

    /// Opens the directory `name` in `parent` with no link followed, creating it first when it
    /// is absent. `-1` on failure, with `errno` set.
    ///
    /// # Safety
    /// `parent` is open. Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn open_or_create_dir(parent: libc::c_int, name: &CString) -> libc::c_int {
        let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = libc::openat(parent, name.as_ptr(), flags);
        if fd >= 0 || *libc::__errno_location() != libc::ENOENT {
            return fd;
        }
        if libc::mkdirat(parent, name.as_ptr(), 0o700) != 0
            && *libc::__errno_location() != libc::EEXIST
        {
            return -1;
        }
        libc::openat(parent, name.as_ptr(), flags)
    }

    /// Opens the regular file `name` in `parent` with no link followed, creating it empty first
    /// when it is absent. `-1` on failure, with `errno` set.
    ///
    /// # Safety
    /// `parent` is open. Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn open_or_create_file(parent: libc::c_int, name: &CString) -> libc::c_int {
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = libc::openat(parent, name.as_ptr(), flags);
        if fd >= 0 || *libc::__errno_location() != libc::ENOENT {
            return fd;
        }
        let created = libc::openat(
            parent,
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        );
        if created >= 0 {
            libc::close(created);
        } else if *libc::__errno_location() != libc::EEXIST {
            return -1;
        }
        libc::openat(parent, name.as_ptr(), flags)
    }

    /// Remounts the bind whose root `fd` names read-only and `nosuid`, keeping the flags its
    /// source mount locks.
    ///
    /// # Safety
    /// `fd` is open on the bind's root. Allocation-free; callable post-`fork()`, pre-exec.
    unsafe fn remount_fd_read_only(fd: libc::c_int) -> Result<(), ()> {
        let mut path = [0u8; PROC_FD_PATH_LEN];
        proc_fd_path(fd, &mut path)?;
        let path = std::ffi::CStr::from_bytes_until_nul(&path).map_err(|_| ())?;
        remount_read_only(path)
    }

    /// Step 5 of [`map_private_tmp`] for every re-exposure, beneath the new `/tmp` at `new_tmp`.
    ///
    /// # Safety
    /// On [`map_private_tmp`]'s terms, after step 3 took every source descriptor.
    unsafe fn reexpose_all(spec: &TmpMapSpec, new_tmp: libc::c_int) -> Result<(), TmpMapFailure> {
        for (index, entry) in spec.reexposures.iter().enumerate() {
            let Some((last, leading)) = entry.components.split_last() else {
                continue;
            };
            // The directories leading to the mountpoint, each reached from the one before.
            let mut parent = new_tmp;
            for component in leading {
                let next = open_or_create_dir(parent, component);
                let failure = (next < 0).then(|| TmpMapFailure::at(Step::CreateMountpoint(index)));
                if parent != new_tmp {
                    close_keeping_errno(parent);
                }
                if let Some(failure) = failure {
                    return Err(failure);
                }
                parent = next;
            }
            let close_parent = |parent: libc::c_int| {
                if parent != new_tmp {
                    // SAFETY: `parent` is a descriptor this loop opened and has not closed.
                    unsafe { close_keeping_errno(parent) };
                }
            };
            let mountpoint = if entry.directory {
                open_or_create_dir(parent, last)
            } else {
                open_or_create_file(parent, last)
            };
            if mountpoint < 0 {
                let failure = TmpMapFailure::at(Step::CreateMountpoint(index));
                close_parent(parent);
                return Err(failure);
            }
            if !fd_kind_is(mountpoint, entry.directory) {
                libc::close(mountpoint);
                close_parent(parent);
                *libc::__errno_location() = libc::ENOTDIR;
                return Err(TmpMapFailure::at(Step::OpenMountpoint(index)));
            }
            let bound = bind_fd_onto_fd(entry.fd.load(Ordering::Relaxed), mountpoint);
            let failure = bound
                .is_err()
                .then(|| TmpMapFailure::at(Step::BindReexposure(index)));
            close_keeping_errno(mountpoint);
            if let Some(failure) = failure {
                close_parent(parent);
                return Err(failure);
            }
            if entry.read_only {
                // The bind's own root, reached through the name now covered by it.
                let root = libc::openat(
                    parent,
                    last.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                );
                let remounted = root >= 0 && remount_fd_read_only(root).is_ok();
                let failure = (!remounted).then(|| TmpMapFailure::at(Step::RemountReadOnly(index)));
                if root >= 0 {
                    close_keeping_errno(root);
                }
                if let Some(failure) = failure {
                    close_parent(parent);
                    return Err(failure);
                }
            }
            close_parent(parent);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the per-tier table, through the one decision function.
    #[test]
    fn temp_mapping_follows_the_per_tier_table() {
        let elsewhere = Path::new("/nonexistent-murmur-temp-dir-workdir");
        let tmp = canonical_tmp();
        let rows = [
            (
                EnforcementTier::KernelSealed,
                true,
                elsewhere,
                TempMapping::ComposedRoot,
            ),
            (
                EnforcementTier::KernelSealed,
                true,
                tmp.as_path(),
                TempMapping::ComposedRoot,
            ),
            (
                EnforcementTier::KernelFull,
                true,
                elsewhere,
                TempMapping::MountNamespace,
            ),
            (
                EnforcementTier::KernelFull,
                false,
                elsewhere,
                TempMapping::NotMapped(NotMappedReason::NoMountNamespace),
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                true,
                elsewhere,
                TempMapping::MountNamespace,
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                false,
                elsewhere,
                TempMapping::NotMapped(NotMappedReason::NoMountNamespaceNoLandlock),
            ),
            (
                EnforcementTier::EnvironmentOnly,
                true,
                elsewhere,
                TempMapping::NotMapped(NotMappedReason::NoKernelContainment),
            ),
            (
                EnforcementTier::EnvironmentOnly,
                false,
                elsewhere,
                TempMapping::NotMapped(NotMappedReason::NoKernelContainment),
            ),
            (
                EnforcementTier::KernelFull,
                true,
                tmp.as_path(),
                TempMapping::NotMapped(NotMappedReason::WorkdirIsTmp),
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                true,
                Path::new(TMP_PATH),
                TempMapping::NotMapped(NotMappedReason::WorkdirIsTmp),
            ),
        ];
        for (tier, usable, workdir, expected) in rows {
            assert_eq!(
                temp_mapping(tier, usable, workdir),
                expected,
                "{tier:?}, mount namespace usable: {usable}, workdir {}",
                workdir.display()
            );
        }
    }

    #[test]
    fn tmpdir_is_tmp_when_mapped_and_the_folder_otherwise() {
        let folder = Path::new("/home/u/.murmur/records/w/ses_1/tmp");
        assert_eq!(TempMapping::ComposedRoot.tmpdir_value(folder), "/tmp");
        assert_eq!(TempMapping::MountNamespace.tmpdir_value(folder), "/tmp");
        for reason in [
            NotMappedReason::NoKernelContainment,
            NotMappedReason::NoMountNamespaceNoLandlock,
            NotMappedReason::NoMountNamespace,
            NotMappedReason::WorkdirIsTmp,
        ] {
            let mapping = TempMapping::NotMapped(reason);
            assert_eq!(mapping.tmpdir_value(folder), folder.to_str().unwrap());
            assert_eq!(mapping.mapped_by(), None);
            assert!(!reason.describe().is_empty());
        }
        assert_eq!(TempMapping::ComposedRoot.mapped_by(), Some("composed-root"));
        assert_eq!(
            TempMapping::MountNamespace.mapped_by(),
            Some("mount-namespace")
        );
    }

    /// Every path strictly beneath `/tmp` is re-exposed at its own path, read-only unless it is
    /// writable, once, in path order; one beneath a writable path is dropped, a writable one
    /// beneath a read-only path is kept; `/tmp` itself, a path elsewhere and one that does not
    /// exist contribute nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn reexposed_paths_are_exact_and_read_only_unless_writable() {
        let tmp = canonical_tmp();
        let base = tempfile::tempdir_in(&tmp).unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        let workdir = base.join("work");
        let session = workdir.join(".murmur").join("ses");
        let records = base.join("records");
        let inside_records = records.join("nested-work");
        let grant = base.join("bin").join("tool");
        for dir in [
            &session,
            &inside_records,
            &grant.parent().unwrap().to_path_buf(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&grant, b"x").unwrap();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let absent = tmp.join("murmur-temp-dir-test-absent");

        let paths = reexposed_paths(
            &tmp,
            [
                workdir.as_path(),
                session.as_path(),
                inside_records.as_path(),
            ],
            [
                records.as_path(),
                grant.as_path(),
                workdir.join(".murmur").as_path(),
                tmp.as_path(),
                home.as_path(),
                absent.as_path(),
                grant.as_path(),
            ],
        );
        assert_eq!(
            paths,
            vec![
                (grant.clone(), true),
                (records.clone(), true),
                (inside_records.clone(), false),
                (workdir.clone(), false),
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_mapping_failure_names_its_step_and_errno() {
        let spec = TmpMapSpec::new(
            Path::new("/home/u/.murmur/records/w/ses_1/tmp"),
            (1, 2),
            Path::new("/tmp"),
            &[],
            Path::new("/work"),
        )
        .unwrap();
        let message = spec.describe(TmpMapFailure::forced(libc::EPERM));
        assert!(message.starts_with(TMP_MAP_FAILURE_PREFIX), "{message}");
        assert!(
            message.contains("unshare(CLONE_NEWNS) failed: "),
            "{message}"
        );
        assert!(
            message.contains(&std::io::Error::from_raw_os_error(libc::EPERM).to_string()),
            "{message}"
        );
    }
}
