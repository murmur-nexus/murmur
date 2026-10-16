//! Every runtime write, append, create, removal and decision-read inside a capsule's workdir,
//! performed relative to a held directory descriptor with no symbolic link followed.
//!
//! The capsule's shell commands can write anywhere in the accessible workdir, and without
//! `--workdir` the session directory *is* that workdir. The runtime itself is unconfined, so a
//! path it builds by joining names onto the workdir and hands to `std::fs` resolves whatever the
//! capsule planted there: `task.md -> ~/victim.txt` turns the runtime's task write into a write to
//! the operator's file, and `tools -> ~/.local/bin` turns a tool install into an install outside
//! the capsule. [`WorkdirRoot`] is the one way the runtime touches those paths.
//!
//! # The walk
//!
//! A relative path is split on `/`; a segment that is empty, `.` or `..` is refused before any
//! syscall, which also refuses an absolute path. Each directory segment is opened with
//! `openat(parent, name, O_DIRECTORY | O_NOFOLLOW)` from the descriptor the previous segment
//! produced, starting at the anchor's own descriptor, so no step resolves a name the capsule could
//! have swapped for a link and no step resolves anything from the anchor's host path. A segment
//! that fails to open is `fstatat(AT_SYMLINK_NOFOLLOW)`-ed to say whether it was a link.
//!
//! This per-component walk is the only implementation, on Linux and macOS alike, for the reason
//! [`crate::resource_plane`] keeps one portable resolution path: `openat2(RESOLVE_BENEATH)` is
//! Linux-only and could only ever be an extra layer over a portable path that has to be correct by
//! itself. Every primitive is `rustix`'s safe wrapper over the POSIX `*at` call of the same name.
//!
//! # Paths in refusals
//!
//! A [`WorkdirRefusal`] names its paths relative to the **accessible** workdir with the real
//! session id — `.murmur/ses_…/plans/plan-1.json`, the form `peer_file_fetch.stored_path` uses —
//! whichever root the call went through. Without `--workdir` the two roots are the same directory
//! and a session path has no `.murmur/<id>/` prefix.
//!
//! # The refusal latch
//!
//! Every refusal is also pushed onto a latch every clone of both roots shares. A best-effort
//! writer that swallows its error (a hook log, a blob, `MURMUR.md`) still leaves the refusal
//! there, and the task loop drains it with [`WorkdirRoot::take_refusals`] at its checkpoints and
//! fails the task.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode};
use rustix::io::Errno;
use serde::Serialize;

/// Mode for a directory created with no reason to choose another: `std::fs::create_dir`'s, so the
/// umask decides the result exactly as it does for `std`.
pub const DEFAULT_DIR_MODE: u32 = 0o777;

/// Mode for a file written with no reason to choose another: `std::fs::File::create`'s, on the
/// same terms as [`DEFAULT_DIR_MODE`].
pub const DEFAULT_FILE_MODE: u32 = 0o666;

/// What a refused call was trying to do. The wire names are what `workdir_path_refused.operation`
/// carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkdirOperation {
    Write,
    Append,
    Read,
    CreateDir,
    Remove,
}

impl WorkdirOperation {
    fn verb(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Append => "append to",
            Self::Read => "read",
            Self::CreateDir => "create directory",
            Self::Remove => "remove",
        }
    }
}

/// Why a call was refused. The wire names are what `workdir_path_refused.reason` carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// `refused_at` is a symbolic link.
    Symlink,
    /// A file being appended to or rewritten in place has more than one name, so it may also be
    /// a file outside the workdir.
    HardLink,
    /// The target is a FIFO, socket, device node or directory where a regular file belongs.
    NotARegularFile,
    /// The relative path has an empty, `.` or `..` segment, or is absolute.
    InvalidComponent,
}

/// One refused call. `path` and `refused_at` are relative to the accessible workdir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkdirRefusal {
    /// The path the runtime meant to touch.
    pub path: String,
    /// The component found to be a link or the wrong file type; the whole path for an invalid
    /// component.
    pub refused_at: String,
    pub operation: WorkdirOperation,
    pub reason: RefusalReason,
}

impl fmt::Display for WorkdirRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.reason {
            RefusalReason::Symlink => "is a symbolic link inside the workdir",
            RefusalReason::HardLink => {
                "is a hard link with another name that may be outside the workdir"
            }
            RefusalReason::NotARegularFile => "is not a regular file",
            RefusalReason::InvalidComponent => "is not a plain relative path inside the workdir",
        };
        write!(
            f,
            "refused to {} {}: {} {what}",
            self.operation.verb(),
            self.path,
            self.refused_at
        )
    }
}

/// Error from a [`WorkdirRoot`] call.
#[derive(Debug, thiserror::Error)]
pub enum WorkdirError {
    /// The call met a link or the wrong file type and did nothing. Already latched.
    #[error("{0}")]
    Refused(WorkdirRefusal),
    /// The call failed for an ordinary I/O reason. `path` is in [`WorkdirRoot::display_path`]'s
    /// form, or the host path when a root opened by path could not be opened.
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl WorkdirError {
    /// Whether the call failed because the path does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

impl From<WorkdirError> for std::io::Error {
    /// A refusal becomes `PermissionDenied` carrying the refusal's text, for the callers whose
    /// own error type is `io::Error`. The refusal itself is already on the latch.
    fn from(error: WorkdirError) -> Self {
        match error {
            WorkdirError::Refused(refusal) => {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, refusal.to_string())
            }
            WorkdirError::Io { source, .. } => source,
        }
    }
}

/// What `fstatat(AT_SYMLINK_NOFOLLOW)` found at a path's last component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    fn of(file_type: FileType) -> Self {
        match file_type {
            FileType::RegularFile => Self::File,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// A held directory descriptor inside (or at) the accessible workdir, and the only way the
/// runtime reads its own decisions from, or writes into, that tree.
///
/// Cheap to clone. Every clone of the accessible root and of the session root derived from it
/// shares one refusal latch.
#[derive(Clone)]
pub struct WorkdirRoot {
    anchor: Arc<Anchor>,
    latch: Arc<Mutex<Vec<WorkdirRefusal>>>,
}

struct Anchor {
    /// Swapped only by [`WorkdirRoot::anchor_fd`], when the directory it names has been removed.
    fd: Mutex<Arc<OwnedFd>>,
    path: PathBuf,
    /// This root's path relative to the accessible root, `""` for the accessible root itself.
    prefix: String,
    origin: Origin,
}

/// How a root reaches its directory again once the one it holds has been removed — a later
/// launch's `trace.retain` pruning a session directory that is still running, for one.
enum Origin {
    /// Opened by its host path, the operator's choice: created and opened by that path again.
    Path,
    /// Walked to from another root: walked to the same way again, creating what is missing and
    /// following no link.
    Beneath { from: WorkdirRoot, rel: String },
}

impl fmt::Debug for WorkdirRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkdirRoot")
            .field("path", &self.anchor.path)
            .field("prefix", &self.anchor.prefix)
            .finish()
    }
}

/// The descriptor a walk stopped at: the anchor's own, or one the walk opened.
enum Held {
    Anchor(Arc<OwnedFd>),
    Opened(OwnedFd),
}

impl AsFd for Held {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Anchor(fd) => fd.as_fd(),
            Self::Opened(fd) => fd.as_fd(),
        }
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

#[cfg(test)]
thread_local! {
    /// Runs between `write_file`'s link check and its rename, so a test can plant a link in the
    /// window a racing capsule would use.
    static BEFORE_RENAME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Makes [`WorkdirRoot::anchor_fd`] ignore `st_nlink`, as on a platform whose `fstat` does not
    /// report a removed directory, so a test reaches [`WorkdirRoot::retry_if_removed`] alone.
    static NLINK_UNRELIABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn nlink_unreliable() -> bool {
    #[cfg(test)]
    {
        NLINK_UNRELIABLE.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Arms [`BEFORE_RENAME`] for the next `write_file` on this thread.
#[cfg(test)]
pub(crate) fn before_next_rename(hook: impl FnOnce() + 'static) {
    BEFORE_RENAME.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

/// Opens the directory at `path` as given, links included.
fn open_by_path(path: &Path) -> Result<OwnedFd, WorkdirError> {
    rustix::fs::open(path, DIR_FLAGS.difference(OFlags::NOFOLLOW), Mode::empty()).map_err(|errno| {
        WorkdirError::Io {
            path: path.display().to_string(),
            source: errno.into(),
        }
    })
}

fn mode_of(mode: u32) -> Mode {
    Mode::from_raw_mode(mode as RawMode)
}

impl WorkdirRoot {
    /// Opens the accessible workdir at `path`.
    ///
    /// `path` is the operator's choice (`--workdir`, or the directory the runtime created under
    /// the manifest directory) and is resolved as given, links above it included. Nothing below
    /// it is ever resolved by path again.
    pub fn open(path: &Path) -> Result<Self, WorkdirError> {
        Ok(Self {
            anchor: Arc::new(Anchor {
                fd: Mutex::new(Arc::new(open_by_path(path)?)),
                path: path.to_path_buf(),
                prefix: String::new(),
                origin: Origin::Path,
            }),
            latch: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// The session root `.murmur/<session_id>` beneath this accessible root, created if absent
    /// and reached with no link followed.
    ///
    /// The returned root holds its own descriptor, so swapping `.murmur` or the session directory
    /// for a link afterwards does not redirect anything written through it.
    pub fn session_beneath(&self, session_id: &str) -> Result<Self, WorkdirError> {
        let rel = format!(".murmur/{session_id}");
        let segments = self.segments(&rel, WorkdirOperation::CreateDir)?;
        let held = self.retry_if_removed(|| {
            self.walk(
                &rel,
                &segments,
                segments.len(),
                WorkdirOperation::CreateDir,
                Some(DEFAULT_DIR_MODE),
            )
        })?;
        let Some(Held::Opened(fd)) = held else {
            unreachable!("a walk that creates always reaches its last segment")
        };
        Ok(Self {
            anchor: Arc::new(Anchor {
                fd: Mutex::new(Arc::new(fd)),
                path: self.anchor.path.join(".murmur").join(session_id),
                prefix: self.display(&rel),
                origin: Origin::Beneath {
                    from: self.clone(),
                    rel,
                },
            }),
            latch: Arc::clone(&self.latch),
        })
    }

    /// The existing directory `rel` beneath this root as a root of its own, reached with no link
    /// followed and sharing this root's latch, or `None` when it or a parent is absent.
    pub fn open_beneath(&self, rel: &str) -> Result<Option<Self>, WorkdirError> {
        let op = WorkdirOperation::Read;
        let segments = self.segments(rel, op)?;
        let Some(held) = self.walk(rel, &segments, segments.len(), op, None)? else {
            return Ok(None);
        };
        let Held::Opened(fd) = held else {
            unreachable!("a walk over at least one segment opens a descriptor")
        };
        Ok(Some(Self {
            anchor: Arc::new(Anchor {
                fd: Mutex::new(Arc::new(fd)),
                path: self.anchor.path.join(rel),
                prefix: self.display(rel),
                origin: Origin::Beneath {
                    from: self.clone(),
                    rel: rel.to_string(),
                },
            }),
            latch: Arc::clone(&self.latch),
        }))
    }

    /// [`Self::open`], sharing `latch_of`'s refusal latch: for a directory outside the accessible
    /// workdir whose refusals still fail this session's task. Its refusals name paths relative to
    /// `path` itself.
    pub fn open_sharing_latch(path: &Path, latch_of: &WorkdirRoot) -> Result<Self, WorkdirError> {
        let opened = Self::open(path)?;
        Ok(Self {
            anchor: opened.anchor,
            latch: Arc::clone(&latch_of.latch),
        })
    }

    /// [`Self::open_sharing_latch`], with every refusal and error naming its path as
    /// `<path>/<rel>`: for a directory outside the workdir that an operator reads about by its
    /// absolute path, such as the session's runtime directory.
    pub fn open_sharing_latch_absolute(
        path: &Path,
        latch_of: &WorkdirRoot,
    ) -> Result<Self, WorkdirError> {
        let opened = Self::open(path)?;
        Ok(Self {
            anchor: Arc::new(Anchor {
                fd: Mutex::new(opened.anchor_fd(false)?),
                path: path.to_path_buf(),
                prefix: path.display().to_string(),
                origin: Origin::Path,
            }),
            latch: Arc::clone(&latch_of.latch),
        })
    }

    /// `(st_dev, st_ino)` of this root's directory, from `fstat` on the held descriptor.
    pub fn identity(&self) -> Result<(u64, u64), WorkdirError> {
        let stat = rustix::fs::fstat(&*self.anchor_fd(false)?)
            .map_err(|errno| self.io("", errno.into()))?;
        Ok((stat.st_dev as u64, stat.st_ino as u64))
    }

    /// A fresh read-only directory descriptor for this root's own directory, reopened from the
    /// held descriptor with no path lookup, for `*at` calls the root has no method for.
    pub fn dir_fd(&self) -> Result<OwnedFd, WorkdirError> {
        let anchor = self.anchor_fd(false)?;
        rustix::fs::openat(&*anchor, ".", DIR_FLAGS, Mode::empty())
            .map_err(|errno| self.io("", errno.into()))
    }

    /// An `O_PATH | O_DIRECTORY` descriptor for this root's own directory, reopened from the held
    /// descriptor with no path lookup: a Landlock rule's fd, or a bind source through
    /// `/proc/self/fd/<n>`.
    #[cfg(target_os = "linux")]
    pub fn path_fd(&self) -> Result<OwnedFd, WorkdirError> {
        let anchor = self.anchor_fd(false)?;
        rustix::fs::openat(
            &*anchor,
            ".",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| self.io("", errno.into()))
    }

    /// An `O_PATH | O_DIRECTORY | O_NOFOLLOW` descriptor for the existing directory `rel`,
    /// reached by the same no-follow walk every other call takes. A link at any segment is
    /// refused and latched; a missing directory is a `NotFound` I/O error.
    #[cfg(target_os = "linux")]
    pub fn path_fd_beneath(&self, rel: impl AsRef<Path>) -> Result<OwnedFd, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Read;
        let (parent, name) = self.parent_of(&rel, op, false)?;
        let Some(parent) = parent else {
            return Err(self.io(&rel, std::io::ErrorKind::NotFound.into()));
        };
        let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let refused_or = |errno: Errno| match self.leaf_type(&parent, name, &rel) {
            Ok(Some(FileType::Symlink)) => self.refuse(&rel, &rel, op, RefusalReason::Symlink),
            Ok(_) => self.io(&rel, errno.into()),
            Err(error) => error,
        };
        let fd = rustix::fs::openat(&parent, name, flags, Mode::empty()).map_err(refused_or)?;
        // `O_PATH | O_NOFOLLOW` opens a link itself rather than failing on it, so the kind is
        // checked on the descriptor that was opened.
        let stat = rustix::fs::fstat(&fd).map_err(|errno| self.io(&rel, errno.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(refused_or(Errno::NOTDIR));
        }
        Ok(fd)
    }

    /// This root's host path, for display, a subprocess argument, a Landlock rule or a bind
    /// source. The runtime never opens it through `std`.
    pub fn path(&self) -> &Path {
        &self.anchor.path
    }

    /// The host path of `rel` beneath this root, on [`Self::path`]'s terms.
    pub fn host_path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.anchor.path.join(rel)
    }

    /// `rel` relative to the accessible workdir, the form every refusal and error uses.
    pub fn display_path(&self, rel: impl AsRef<Path>) -> String {
        self.display(&rel.as_ref().to_string_lossy())
    }

    /// The uid and gid that own this root's directory, from `fstat` on the held descriptor.
    pub fn owner(&self) -> Result<(u32, u32), WorkdirError> {
        let stat = rustix::fs::fstat(&*self.anchor_fd(false)?)
            .map_err(|errno| self.io("", errno.into()))?;
        Ok((stat.st_uid, stat.st_gid))
    }

    /// Every refusal latched since the last call, on any clone of either root, oldest first.
    pub fn take_refusals(&self) -> Vec<WorkdirRefusal> {
        std::mem::take(&mut *self.latch.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Writes `bytes` to `rel` through a same-directory temporary file renamed over it.
    ///
    /// Missing parent directories are created with [`DEFAULT_DIR_MODE`]. A link at `rel` when the
    /// call starts is refused; one planted after that check is replaced by the rename, never
    /// followed. A directory at `rel` is an I/O error. The temporary file
    /// `.<name>.<uuid v7 simple>.tmp` is removed on any failure.
    pub fn write_file(
        &self,
        rel: impl AsRef<Path>,
        bytes: &[u8],
        mode: u32,
    ) -> Result<(), WorkdirError> {
        self.retry_if_removed(|| self.write_file_inner(rel.as_ref(), bytes, mode, false))
    }

    fn write_file_inner(
        &self,
        rel: &Path,
        bytes: &[u8],
        mode: u32,
        exact_mode: bool,
    ) -> Result<(), WorkdirError> {
        let rel = rel.to_string_lossy();
        let op = WorkdirOperation::Write;
        let (parent, name) = self.parent_of(&rel, op, true)?;
        let parent = parent.expect("a creating walk reaches the parent");
        match self.leaf_type(&parent, name, &rel)? {
            Some(FileType::Symlink) => {
                return Err(self.refuse(&rel, &rel, op, RefusalReason::Symlink));
            }
            Some(FileType::Directory) => {
                return Err(self.io(&rel, std::io::ErrorKind::IsADirectory.into()));
            }
            _ => {}
        }
        let temp = format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            uuid::Uuid::now_v7().simple()
        );
        let flags =
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&parent, temp.as_str(), flags, mode_of(mode))
            .map_err(|errno| self.io(&rel, errno.into()))?;
        let chmod = if exact_mode {
            rustix::fs::fchmod(&fd, mode_of(mode)).map_err(|errno| self.io(&rel, errno.into()))
        } else {
            Ok(())
        };
        let result = chmod
            .and_then(|()| {
                File::from(fd)
                    .write_all(bytes)
                    .map_err(|source| self.io(&rel, source))
            })
            .and_then(|()| {
                #[cfg(test)]
                if let Some(hook) = BEFORE_RENAME.with(|slot| slot.borrow_mut().take()) {
                    hook();
                }
                rustix::fs::renameat(&parent, temp.as_str(), &parent, name)
                    .map_err(|errno| self.io(&rel, errno.into()))
            });
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&parent, temp.as_str(), AtFlags::empty());
        }
        result
    }

    /// [`Self::write_file`], with the file's mode set to exactly `mode` — `fchmod` on the
    /// temporary file before the rename — rather than `mode` less the umask.
    pub fn write_file_exact_mode(
        &self,
        rel: impl AsRef<Path>,
        bytes: &[u8],
        mode: u32,
    ) -> Result<(), WorkdirError> {
        self.retry_if_removed(|| self.write_file_inner(rel.as_ref(), bytes, mode, true))
    }

    /// Truncates and rewrites the regular file at `rel` in place, creating it with `mode` if
    /// absent, so its inode survives — what a Landlock rule bound to that inode needs.
    ///
    /// A link, a non-regular file or a file with more than one hard link at `rel` is refused.
    pub fn overwrite_in_place(
        &self,
        rel: impl AsRef<Path>,
        bytes: &[u8],
        mode: u32,
    ) -> Result<(), WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Write;
        let flags = OFlags::WRONLY | OFlags::CREATE;
        let mut file = self.retry_if_removed(|| self.open_regular(&rel, op, flags, mode, true))?;
        file.set_len(0).map_err(|source| self.io(&rel, source))?;
        file.write_all(bytes)
            .map_err(|source| self.io(&rel, source))
    }

    /// Opens `rel` for appending, creating it with `mode` and its parents with
    /// [`DEFAULT_DIR_MODE`] if absent.
    ///
    /// Refused when `rel` is a link, is not a regular file, or is a regular file with more than
    /// one hard link — the last because a hard link to a host file would otherwise be appended to.
    pub fn open_append(&self, rel: impl AsRef<Path>, mode: u32) -> Result<File, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let flags = OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE;
        self.retry_if_removed(|| {
            self.open_regular(&rel, WorkdirOperation::Append, flags, mode, true)
        })
    }

    /// The bytes of the regular file at `rel`, or `None` when it or a parent is absent.
    ///
    /// Refused when `rel` or a parent is a link, or `rel` is not a regular file; a FIFO is
    /// refused without blocking. A file longer than `max_bytes` is an `InvalidData` error.
    pub fn read_file(
        &self,
        rel: impl AsRef<Path>,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let mut file =
            match self.open_regular(&rel, WorkdirOperation::Read, OFlags::RDONLY, 0, false) {
                Ok(file) => file,
                Err(error) if error.is_not_found() => return Ok(None),
                Err(error) => return Err(error),
            };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|source| self.io(&rel, source))?;
        if bytes.len() as u64 > max_bytes {
            return Err(self.io(
                &rel,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("larger than {max_bytes} bytes"),
                ),
            ));
        }
        Ok(Some(bytes))
    }

    /// [`Self::read_file`] with no size bound, decoded as UTF-8.
    pub fn read_to_string(&self, rel: impl AsRef<Path>) -> Result<Option<String>, WorkdirError> {
        let rel = rel.as_ref();
        let Some(bytes) = self.read_file(rel, u64::MAX)? else {
            return Ok(None);
        };
        String::from_utf8(bytes).map(Some).map_err(|error| {
            self.io(
                &rel.to_string_lossy(),
                std::io::Error::new(std::io::ErrorKind::InvalidData, error),
            )
        })
    }

    /// Creates `rel` and every missing parent. `mode` applies to each directory this call
    /// creates (less the umask); an existing one keeps its mode. A link at any segment is refused.
    pub fn create_dir_all(&self, rel: impl AsRef<Path>, mode: u32) -> Result<(), WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::CreateDir;
        let segments = self.segments(&rel, op)?;
        self.retry_if_removed(|| self.walk(&rel, &segments, segments.len(), op, Some(mode)))
            .map(|_| ())
    }

    /// What `fstatat(AT_SYMLINK_NOFOLLOW)` finds at `rel`, or `None` when it or a parent is
    /// absent. A link at a parent is refused; a link at `rel` itself is reported as
    /// [`EntryKind::Symlink`] for the caller to refuse.
    pub fn entry_kind(&self, rel: impl AsRef<Path>) -> Result<Option<EntryKind>, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Read;
        let (parent, name) = self.parent_of(&rel, op, false)?;
        let Some(parent) = parent else {
            return Ok(None);
        };
        Ok(self.leaf_type(&parent, name, &rel)?.map(EntryKind::of))
    }

    /// Size in bytes of the regular file at `rel`, or `None` when it or a parent is absent or it
    /// is not a regular file. A link at any segment is refused.
    pub fn file_len(&self, rel: impl AsRef<Path>) -> Result<Option<u64>, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Read;
        let (parent, name) = self.parent_of(&rel, op, false)?;
        let Some(parent) = parent else {
            return Ok(None);
        };
        match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Ok(Some(stat.st_size as u64)),
                FileType::Symlink => Err(self.refuse(&rel, &rel, op, RefusalReason::Symlink)),
                _ => Ok(None),
            },
            Err(Errno::NOENT) => Ok(None),
            Err(errno) => Err(self.io(&rel, errno.into())),
        }
    }

    /// The entries of the directory `rel` (`""` for this root itself) with what each is, sorted by
    /// name, or `None` when it or a parent is absent. A link at any segment of `rel` is refused;
    /// a link *inside* it is listed as [`EntryKind::Symlink`].
    pub fn list_dir(
        &self,
        rel: impl AsRef<Path>,
    ) -> Result<Option<Vec<(String, EntryKind)>>, WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Read;
        let held = if rel.is_empty() {
            Some(Held::Anchor(self.anchor_fd(false)?))
        } else {
            let segments = self.segments(&rel, op)?;
            self.walk(&rel, &segments, segments.len(), op, None)?
        };
        let Some(dir) = held else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        for (name, file_type) in self.children(&dir, &rel)? {
            entries.push((
                name.to_string_lossy().into_owned(),
                EntryKind::of(file_type),
            ));
        }
        entries.sort();
        Ok(Some(entries))
    }

    /// Unlinks the file at `rel`. A link at `rel` or at a parent is refused rather than removed,
    /// so the refusal is recorded.
    pub fn remove_file(&self, rel: impl AsRef<Path>) -> Result<(), WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Remove;
        let (parent, name) = self.parent_of(&rel, op, false)?;
        let Some(parent) = parent else {
            return Err(self.io(&rel, std::io::ErrorKind::NotFound.into()));
        };
        if self.leaf_type(&parent, name, &rel)? == Some(FileType::Symlink) {
            return Err(self.refuse(&rel, &rel, op, RefusalReason::Symlink));
        }
        rustix::fs::unlinkat(&parent, name, AtFlags::empty())
            .map_err(|errno| self.io(&rel, errno.into()))
    }

    /// Removes the directory `rel` and everything in it, by descriptor: each child directory is
    /// opened no-follow from its parent's descriptor, so no part of the removal resolves a path
    /// from the root. A link at `rel` or at a parent is refused; a link inside is unlinked, not
    /// followed.
    pub fn remove_dir_all(&self, rel: impl AsRef<Path>) -> Result<(), WorkdirError> {
        let rel = rel.as_ref().to_string_lossy();
        let op = WorkdirOperation::Remove;
        let (parent, name) = self.parent_of(&rel, op, false)?;
        let Some(parent) = parent else {
            return Err(self.io(&rel, std::io::ErrorKind::NotFound.into()));
        };
        match self.leaf_type(&parent, name, &rel)? {
            None => return Err(self.io(&rel, std::io::ErrorKind::NotFound.into())),
            Some(FileType::Symlink) => {
                return Err(self.refuse(&rel, &rel, op, RefusalReason::Symlink));
            }
            Some(FileType::Directory) => {}
            Some(_) => return Err(self.io(&rel, std::io::ErrorKind::NotADirectory.into())),
        }
        let dir = self.open_dir(&parent, name, &rel, &rel, op)?;
        self.empty_dir(&dir, &rel)?;
        rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR)
            .map_err(|errno| self.io(&rel, errno.into()))
    }

    fn empty_dir(&self, dir: &OwnedFd, rel: &str) -> Result<(), WorkdirError> {
        for (name, file_type) in self.children(dir, rel)? {
            let child = format!("{rel}/{}", name.to_string_lossy());
            if file_type == FileType::Directory {
                let sub = self.open_dir(dir, &name, &child, &child, WorkdirOperation::Remove)?;
                self.empty_dir(&sub, &child)?;
                rustix::fs::unlinkat(dir, &name, AtFlags::REMOVEDIR)
            } else {
                rustix::fs::unlinkat(dir, &name, AtFlags::empty())
            }
            .map_err(|errno| self.io(&child, errno.into()))?;
        }
        Ok(())
    }

    /// The names in `dir` other than `.` and `..`, each with the type `fstatat(AT_SYMLINK_NOFOLLOW)`
    /// reports — not `d_type`, which some filesystems leave unknown.
    fn children(
        &self,
        dir: &impl AsFd,
        rel: &str,
    ) -> Result<Vec<(std::ffi::OsString, FileType)>, WorkdirError> {
        let mut listing =
            rustix::fs::Dir::read_from(dir).map_err(|errno| self.io(rel, errno.into()))?;
        let mut names = Vec::new();
        while let Some(entry) = listing.read() {
            let entry = entry.map_err(|errno| self.io(rel, errno.into()))?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            names.push(name.to_os_string());
        }
        let mut children = Vec::with_capacity(names.len());
        for name in names {
            match rustix::fs::statat(dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => children.push((name, FileType::from_raw_mode(stat.st_mode))),
                Err(Errno::NOENT) => {}
                Err(errno) => return Err(self.io(rel, errno.into())),
            }
        }
        Ok(children)
    }

    /// Opens `rel` as a regular file with `flags | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`, then
    /// checks the descriptor's own `fstat`: not a regular file is refused, and so is more than
    /// one hard link when `flags` writes.
    fn open_regular(
        &self,
        rel: &str,
        op: WorkdirOperation,
        flags: OFlags,
        mode: u32,
        create_parents: bool,
    ) -> Result<File, WorkdirError> {
        let (parent, name) = self.parent_of(rel, op, create_parents)?;
        let Some(parent) = parent else {
            return Err(self.io(rel, std::io::ErrorKind::NotFound.into()));
        };
        let flags = flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(&parent, name, flags, mode_of(mode)) {
            Ok(fd) => fd,
            Err(errno) => {
                return Err(match self.leaf_type(&parent, name, rel)? {
                    Some(FileType::Symlink) => self.refuse(rel, rel, op, RefusalReason::Symlink),
                    Some(FileType::RegularFile) | None => self.io(rel, errno.into()),
                    Some(_) => self.refuse(rel, rel, op, RefusalReason::NotARegularFile),
                });
            }
        };
        let stat = rustix::fs::fstat(&fd).map_err(|errno| self.io(rel, errno.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(self.refuse(rel, rel, op, RefusalReason::NotARegularFile));
        }
        if op != WorkdirOperation::Read && stat.st_nlink > 1 {
            return Err(self.refuse(rel, rel, op, RefusalReason::HardLink));
        }
        Ok(File::from(fd))
    }

    /// Splits `rel` on `/`, refusing an empty, `.` or `..` segment — and so an absolute path —
    /// before any syscall.
    fn segments<'r>(
        &self,
        rel: &'r str,
        op: WorkdirOperation,
    ) -> Result<Vec<&'r OsStr>, WorkdirError> {
        let segments: Vec<&OsStr> = rel.split('/').map(OsStr::new).collect();
        if segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
        {
            return Err(self.refuse(rel, rel, op, RefusalReason::InvalidComponent));
        }
        Ok(segments)
    }

    /// Walks to `rel`'s parent and returns its descriptor with the last segment, or `None` for
    /// the descriptor when a parent is absent and `create` is false.
    fn parent_of<'r>(
        &self,
        rel: &'r str,
        op: WorkdirOperation,
        create: bool,
    ) -> Result<(Option<Held>, &'r OsStr), WorkdirError> {
        let segments = self.segments(rel, op)?;
        let name = *segments.last().expect("split yields at least one segment");
        let held = self.walk(
            rel,
            &segments,
            segments.len() - 1,
            op,
            create.then_some(DEFAULT_DIR_MODE),
        )?;
        Ok((held, name))
    }

    /// Opens the first `depth` segments one at a time from the anchor, creating each missing one
    /// with `create`'s mode when given. `None` when a segment is absent and `create` is `None`.
    fn walk(
        &self,
        rel: &str,
        segments: &[&OsStr],
        depth: usize,
        op: WorkdirOperation,
        create: Option<u32>,
    ) -> Result<Option<Held>, WorkdirError> {
        let mut current = Held::Anchor(self.anchor_fd(create.is_some())?);
        for index in 0..depth {
            let name = segments[index];
            let so_far = segments[..=index]
                .iter()
                .map(|segment| segment.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let next = match rustix::fs::openat(&current, name, DIR_FLAGS, Mode::empty()) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => {
                    let Some(mode) = create else {
                        return Ok(None);
                    };
                    match rustix::fs::mkdirat(&current, name, mode_of(mode)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(errno) => return Err(self.io(&so_far, errno.into())),
                    }
                    self.open_dir(&current, name, rel, &so_far, op)?
                }
                Err(errno) => return Err(self.classify(&current, name, rel, &so_far, op, errno)),
            };
            current = Held::Opened(next);
        }
        Ok(Some(current))
    }

    fn open_dir(
        &self,
        parent: &impl AsFd,
        name: &OsStr,
        rel: &str,
        so_far: &str,
        op: WorkdirOperation,
    ) -> Result<OwnedFd, WorkdirError> {
        rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty())
            .map_err(|errno| self.classify(parent, name, rel, so_far, op, errno))
    }

    /// Turns a failed directory open into a refusal when `fstatat` says the segment is a link,
    /// and into an I/O error otherwise.
    fn classify(
        &self,
        parent: &impl AsFd,
        name: &OsStr,
        rel: &str,
        so_far: &str,
        op: WorkdirOperation,
        errno: Errno,
    ) -> WorkdirError {
        match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                self.refuse(rel, so_far, op, RefusalReason::Symlink)
            }
            _ => self.io(so_far, errno.into()),
        }
    }

    /// The type at `name` in `parent`, not following a link, or `None` when absent.
    fn leaf_type(
        &self,
        parent: &impl AsFd,
        name: &OsStr,
        rel: &str,
    ) -> Result<Option<FileType>, WorkdirError> {
        match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
            Err(Errno::NOENT) => Ok(None),
            Err(errno) => Err(self.io(rel, errno.into())),
        }
    }

    /// The anchor's descriptor. Before a call that creates, a directory `fstat` reports removed
    /// (no links to it) is reached again by its [`Origin`] and the new descriptor held from then
    /// on.
    fn anchor_fd(&self, creating: bool) -> Result<Arc<OwnedFd>, WorkdirError> {
        let mut held = self.anchor.fd.lock().unwrap_or_else(|e| e.into_inner());
        let removed = creating
            && !nlink_unreliable()
            && rustix::fs::fstat(&**held).is_ok_and(|stat| stat.st_nlink == 0);
        if removed {
            *held = Arc::new(self.reach_again()?);
        }
        Ok(Arc::clone(&held))
    }

    /// Runs a creating `call`, and runs it once more when it failed `NotFound` because the held
    /// directory was removed, after reaching that directory again by its [`Origin`].
    ///
    /// `fstat` does not reveal a removed directory on every platform — Darwin keeps its link
    /// count above zero — so [`Self::anchor_fd`]'s check alone would leave a removed root
    /// creating into a directory that no longer exists. A directory that was moved, not removed,
    /// stays held: writes follow it, never the name it used to have.
    fn retry_if_removed<T>(
        &self,
        call: impl Fn() -> Result<T, WorkdirError>,
    ) -> Result<T, WorkdirError> {
        match call() {
            Err(error) if error.is_not_found() && self.reach_again_if_removed()? => call(),
            result => result,
        }
    }

    /// Whether the held directory was removed, found by creating a uniquely named directory in
    /// it and removing it again; when it was, the root holds the directory reached again.
    fn reach_again_if_removed(&self) -> Result<bool, WorkdirError> {
        let mut held = self.anchor.fd.lock().unwrap_or_else(|e| e.into_inner());
        let probe = format!(".{}.probe", uuid::Uuid::now_v7().simple());
        match rustix::fs::mkdirat(&**held, probe.as_str(), Mode::RWXU) {
            Ok(()) => {
                let _ = rustix::fs::unlinkat(&**held, probe.as_str(), AtFlags::REMOVEDIR);
                Ok(false)
            }
            Err(Errno::NOENT) => {
                *held = Arc::new(self.reach_again()?);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// The anchor's directory reached again from its [`Origin`], created if missing: by its host
    /// path for a root opened by path, and by a no-follow walk from the parent root otherwise.
    fn reach_again(&self) -> Result<OwnedFd, WorkdirError> {
        match &self.anchor.origin {
            Origin::Path => {
                std::fs::create_dir_all(&self.anchor.path).map_err(|source| self.io("", source))?;
                open_by_path(&self.anchor.path)
            }
            Origin::Beneath { from, rel } => {
                let op = WorkdirOperation::CreateDir;
                let segments = from.segments(rel, op)?;
                let walked = from.retry_if_removed(|| {
                    from.walk(rel, &segments, segments.len(), op, Some(DEFAULT_DIR_MODE))
                })?;
                let Some(Held::Opened(fd)) = walked else {
                    unreachable!("a walk that creates always reaches its last segment")
                };
                Ok(fd)
            }
        }
    }

    fn display(&self, rel: &str) -> String {
        match (self.anchor.prefix.is_empty(), rel.is_empty()) {
            (true, _) => rel.to_string(),
            (false, true) => self.anchor.prefix.clone(),
            (false, false) => format!("{}/{rel}", self.anchor.prefix),
        }
    }

    fn refuse(
        &self,
        rel: &str,
        refused_at: &str,
        operation: WorkdirOperation,
        reason: RefusalReason,
    ) -> WorkdirError {
        let refusal = WorkdirRefusal {
            path: self.display(rel),
            refused_at: self.display(refused_at),
            operation,
            reason,
        };
        self.latch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(refusal.clone());
        WorkdirError::Refused(refusal)
    }

    fn io(&self, rel: &str, source: std::io::Error) -> WorkdirError {
        WorkdirError::Io {
            path: self.display(rel),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    struct Fixture {
        _temp: tempfile::TempDir,
        workdir: PathBuf,
        outside: PathBuf,
        sentinel: PathBuf,
        root: WorkdirRoot,
    }

    const SENTINEL: &[u8] = b"operator data";

    fn refusal_of(error: &WorkdirError) -> &WorkdirRefusal {
        match error {
            WorkdirError::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path().join("workdir");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&workdir).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel.txt");
        std::fs::write(&sentinel, SENTINEL).unwrap();
        let root = WorkdirRoot::open(&workdir).unwrap();
        Fixture {
            _temp: temp,
            workdir,
            outside,
            sentinel,
            root,
        }
    }

    impl Fixture {
        fn outside_snapshot(&self) -> Vec<(String, Vec<u8>, i64, i64)> {
            let mut entries: Vec<_> = std::fs::read_dir(&self.outside)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let meta = std::fs::symlink_metadata(entry.path()).unwrap();
                    let bytes = if meta.is_file() {
                        std::fs::read(entry.path()).unwrap()
                    } else {
                        Vec::new()
                    };
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        bytes,
                        meta.mtime(),
                        meta.mtime_nsec(),
                    )
                })
                .collect();
            entries.sort();
            entries
        }

        /// Plants a link at `rel` to the sentinel, creating its parent directories.
        fn link_at_target(&self, rel: &str) {
            let host = self.workdir.join(rel);
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            symlink(&self.sentinel, host).unwrap();
        }

        fn dangling_at_target(&self, rel: &str) {
            let host = self.workdir.join(rel);
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            symlink(self.outside.join("not-yet.txt"), host).unwrap();
        }

        fn link_at_parent(&self, parent_rel: &str) {
            let host = self.workdir.join(parent_rel);
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            symlink(&self.outside, host).unwrap();
        }
    }

    type Operation = fn(&WorkdirRoot, &str) -> Result<(), WorkdirError>;

    fn operations() -> Vec<(&'static str, WorkdirOperation, Operation)> {
        vec![
            ("write_file", WorkdirOperation::Write, |root, rel| {
                root.write_file(rel, b"runtime bytes", 0o644)
            }),
            ("open_append", WorkdirOperation::Append, |root, rel| {
                root.open_append(rel, 0o644).and_then(|mut file| {
                    file.write_all(b"runtime bytes")
                        .map_err(|source| WorkdirError::Io {
                            path: rel.to_string(),
                            source,
                        })
                })
            }),
            ("read_file", WorkdirOperation::Read, |root, rel| {
                root.read_file(rel, u64::MAX).map(|_| ())
            }),
            (
                "create_dir_all",
                WorkdirOperation::CreateDir,
                |root, rel| root.create_dir_all(rel, 0o755),
            ),
            ("remove_file", WorkdirOperation::Remove, |root, rel| {
                root.remove_file(rel)
            }),
            ("remove_dir_all", WorkdirOperation::Remove, |root, rel| {
                root.remove_dir_all(rel)
            }),
        ]
    }

    fn assert_refused(
        fixture: &Fixture,
        result: Result<(), WorkdirError>,
        before: &[(String, Vec<u8>, i64, i64)],
        label: &str,
        path: &str,
        refused_at: &str,
        operation: WorkdirOperation,
    ) {
        let expected = WorkdirRefusal {
            path: path.to_string(),
            refused_at: refused_at.to_string(),
            operation,
            reason: RefusalReason::Symlink,
        };
        match result {
            Err(WorkdirError::Refused(refusal)) => assert_eq!(refusal, expected, "{label}"),
            other => panic!("{label}: expected a refusal, got {other:?}"),
        }
        assert_eq!(
            fixture.root.take_refusals(),
            vec![expected],
            "{label}: latched once"
        );
        assert_eq!(
            fixture.outside_snapshot(),
            before,
            "{label}: outside changed"
        );
        assert_eq!(std::fs::read(&fixture.sentinel).unwrap(), SENTINEL);
    }

    #[test]
    fn every_operation_refuses_a_link_at_the_target() {
        for (label, operation, call) in operations() {
            let fixture = fixture();
            fixture.link_at_target("dir/target");
            let before = fixture.outside_snapshot();
            let result = call(&fixture.root, "dir/target");
            assert_refused(
                &fixture,
                result,
                &before,
                label,
                "dir/target",
                "dir/target",
                operation,
            );
            assert!(
                std::fs::symlink_metadata(fixture.workdir.join("dir/target"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn every_operation_refuses_a_link_at_a_parent_directory() {
        for (label, operation, call) in operations() {
            let fixture = fixture();
            std::fs::create_dir(fixture.outside.join("dir")).unwrap();
            std::fs::write(fixture.outside.join("dir/target"), SENTINEL).unwrap();
            fixture.link_at_parent("a/b");
            let before = fixture.outside_snapshot();
            let result = call(&fixture.root, "a/b/dir/target");
            assert_refused(
                &fixture,
                result,
                &before,
                label,
                "a/b/dir/target",
                "a/b",
                operation,
            );
            assert_eq!(
                std::fs::read(fixture.outside.join("dir/target")).unwrap(),
                SENTINEL
            );
        }
    }

    #[test]
    fn every_operation_refuses_a_dangling_link_at_the_target() {
        for (label, operation, call) in operations() {
            let fixture = fixture();
            fixture.dangling_at_target("target");
            let before = fixture.outside_snapshot();
            let result = call(&fixture.root, "target");
            assert_refused(
                &fixture, result, &before, label, "target", "target", operation,
            );
            assert!(!fixture.outside.join("not-yet.txt").exists(), "{label}");
        }
    }

    #[test]
    fn a_non_plain_component_is_refused_before_any_syscall() {
        let fixture = fixture();
        for rel in [
            "",
            "/etc/passwd",
            "../outside/sentinel.txt",
            "a/./b",
            "a//b",
            "a/",
            ".",
        ] {
            for (label, operation, call) in operations() {
                match call(&fixture.root, rel) {
                    Err(WorkdirError::Refused(refusal)) => {
                        assert_eq!(
                            refusal.reason,
                            RefusalReason::InvalidComponent,
                            "{label} {rel}"
                        );
                        assert_eq!(refusal.operation, operation);
                        assert_eq!(refusal.path, rel);
                    }
                    other => panic!("{label} {rel:?}: expected a refusal, got {other:?}"),
                }
                assert_eq!(fixture.root.take_refusals().len(), 1);
            }
        }
        assert_eq!(std::fs::read(&fixture.sentinel).unwrap(), SENTINEL);
        assert_eq!(std::fs::read_dir(&fixture.workdir).unwrap().count(), 0);
    }

    #[test]
    fn a_fifo_is_refused_for_append_and_read_without_blocking() {
        let fixture = fixture();
        let fifo = fixture.workdir.join("pipe");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let append = fixture.root.open_append("pipe", 0o644).map(|_| ());
        let read = fixture.root.read_file("pipe", u64::MAX).map(|_| ());
        for (result, operation) in [
            (append, WorkdirOperation::Append),
            (read, WorkdirOperation::Read),
        ] {
            match result {
                Err(WorkdirError::Refused(refusal)) => {
                    assert_eq!(refusal.reason, RefusalReason::NotARegularFile);
                    assert_eq!(refusal.operation, operation);
                    assert_eq!(refusal.refused_at, "pipe");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(fixture.root.take_refusals().len(), 2);
    }

    #[test]
    fn an_append_to_a_hard_link_into_outside_is_refused() {
        let fixture = fixture();
        std::fs::hard_link(&fixture.sentinel, fixture.workdir.join("log")).unwrap();
        let before = fixture.outside_snapshot();
        match fixture.root.open_append("log", 0o644) {
            Err(WorkdirError::Refused(refusal)) => {
                assert_eq!(refusal.reason, RefusalReason::HardLink);
                assert_eq!(refusal.operation, WorkdirOperation::Append);
                assert_eq!(refusal.path, "log");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(fixture.outside_snapshot(), before);
        assert_eq!(fixture.root.take_refusals().len(), 1);
    }

    #[test]
    fn a_write_replaces_an_existing_file_and_leaves_no_temporary_sibling() {
        let fixture = fixture();
        std::fs::create_dir(fixture.workdir.join("out")).unwrap();
        std::fs::write(fixture.workdir.join("out/result.txt"), b"old").unwrap();
        fixture
            .root
            .write_file("out/result.txt", b"new", 0o600)
            .unwrap();
        assert_eq!(
            std::fs::read(fixture.workdir.join("out/result.txt")).unwrap(),
            b"new"
        );
        let names: Vec<_> = std::fs::read_dir(fixture.workdir.join("out"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("result.txt")]);
        let mode = std::fs::metadata(fixture.workdir.join("out/result.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(fixture.root.take_refusals().is_empty());
    }

    #[test]
    fn a_link_planted_between_the_check_and_the_rename_is_replaced_not_followed() {
        let fixture = fixture();
        let target = fixture.workdir.join("task.md");
        let sentinel = fixture.sentinel.clone();
        let planted = target.clone();
        before_next_rename(move || symlink(&sentinel, &planted).unwrap());
        let before = fixture.outside_snapshot();
        fixture
            .root
            .write_file("task.md", b"the task", 0o644)
            .unwrap();
        assert_eq!(fixture.outside_snapshot(), before);
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.is_file(), "the rename replaced the planted link");
        assert_eq!(std::fs::read(&target).unwrap(), b"the task");
    }

    #[test]
    fn a_murmur_link_planted_after_the_session_root_is_held_redirects_nothing() {
        let fixture = fixture();
        let session = fixture.root.session_beneath("ses_test").unwrap();
        let murmur = fixture.workdir.join(".murmur");
        std::fs::rename(&murmur, fixture.workdir.join("moved")).unwrap();
        symlink(&fixture.outside, &murmur).unwrap();
        let before = fixture.outside_snapshot();
        session
            .write_file("plans/plan-1.json", b"{}", 0o644)
            .unwrap();
        assert_eq!(fixture.outside_snapshot(), before);
        assert_eq!(
            std::fs::read(fixture.workdir.join("moved/ses_test/plans/plan-1.json")).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn a_session_root_refusal_names_the_path_from_the_accessible_workdir_and_shares_the_latch() {
        let fixture = fixture();
        let session = fixture.root.session_beneath("ses_test").unwrap();
        symlink(
            &fixture.outside,
            fixture.workdir.join(".murmur/ses_test/plans"),
        )
        .unwrap();
        let error = session
            .write_file("plans/plan-1.json", b"{}", 0o644)
            .unwrap_err();
        let refusal = refusal_of(&error);
        assert_eq!(refusal.path, ".murmur/ses_test/plans/plan-1.json");
        assert_eq!(refusal.refused_at, ".murmur/ses_test/plans");
        assert_eq!(
            error.to_string(),
            "refused to write .murmur/ses_test/plans/plan-1.json: .murmur/ses_test/plans is a \
             symbolic link inside the workdir"
        );
        assert_eq!(fixture.root.take_refusals(), vec![refusal.clone()]);
        assert!(session.take_refusals().is_empty());
    }

    #[test]
    fn a_murmur_link_at_session_derivation_is_refused() {
        let fixture = fixture();
        symlink(&fixture.outside, fixture.workdir.join(".murmur")).unwrap();
        let error = fixture.root.session_beneath("ses_test").unwrap_err();
        let refusal = refusal_of(&error);
        assert_eq!(refusal.refused_at, ".murmur");
        assert_eq!(refusal.operation, WorkdirOperation::CreateDir);
        assert_eq!(std::fs::read_dir(&fixture.outside).unwrap().count(), 1);
    }

    #[test]
    fn a_removed_session_directory_is_walked_to_again_before_a_write() {
        for nlink_unreliable in [false, true] {
            NLINK_UNRELIABLE.with(|cell| cell.set(nlink_unreliable));
            let fixture = fixture();
            let session = fixture.root.session_beneath("ses_test").unwrap();
            std::fs::remove_dir_all(fixture.workdir.join(".murmur")).unwrap();

            session
                .write_file("out/result.txt", b"answer", 0o644)
                .unwrap();
            session
                .open_append("logs/bootstrap.log", 0o644)
                .unwrap()
                .write_all(b"line\n")
                .unwrap();

            assert_eq!(
                std::fs::read(fixture.workdir.join(".murmur/ses_test/out/result.txt")).unwrap(),
                b"answer"
            );
            assert!(fixture
                .workdir
                .join(".murmur/ses_test/logs/bootstrap.log")
                .is_file());
            assert!(fixture.root.take_refusals().is_empty());
        }
        NLINK_UNRELIABLE.with(|cell| cell.set(false));
    }

    /// A session directory moved aside, not removed, stays the one written to, with no probe
    /// left behind in it.
    #[test]
    fn a_moved_session_directory_is_still_the_one_written_to() {
        NLINK_UNRELIABLE.with(|cell| cell.set(true));
        let fixture = fixture();
        let session = fixture.root.session_beneath("ses_test").unwrap();
        std::fs::rename(
            fixture.workdir.join(".murmur"),
            fixture.workdir.join("moved"),
        )
        .unwrap();

        session
            .write_file("out/result.txt", b"answer", 0o644)
            .unwrap();

        NLINK_UNRELIABLE.with(|cell| cell.set(false));
        assert_eq!(
            std::fs::read(fixture.workdir.join("moved/ses_test/out/result.txt")).unwrap(),
            b"answer"
        );
        assert!(!fixture.workdir.join(".murmur").exists());
        let names: Vec<_> = std::fs::read_dir(fixture.workdir.join("moved/ses_test"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["out"]);
    }

    #[test]
    fn a_removed_session_directory_replaced_by_a_link_is_refused() {
        for nlink_unreliable in [false, true] {
            NLINK_UNRELIABLE.with(|cell| cell.set(nlink_unreliable));
            let fixture = fixture();
            let session = fixture.root.session_beneath("ses_test").unwrap();
            std::fs::remove_dir_all(fixture.workdir.join(".murmur")).unwrap();
            std::os::unix::fs::symlink(&fixture.outside, fixture.workdir.join(".murmur")).unwrap();
            let before = fixture.outside_snapshot();

            let error = session
                .write_file("out/result.txt", b"answer", 0o644)
                .unwrap_err();

            assert_eq!(refusal_of(&error).refused_at, ".murmur");
            assert_eq!(fixture.outside_snapshot(), before);
        }
        NLINK_UNRELIABLE.with(|cell| cell.set(false));
    }

    #[test]
    fn absent_paths_read_as_none_with_no_refusal() {
        let fixture = fixture();
        assert_eq!(fixture.root.read_file("task.md", 10).unwrap(), None);
        assert_eq!(fixture.root.read_to_string("a/b/c").unwrap(), None);
        assert_eq!(fixture.root.entry_kind("a/b").unwrap(), None);
        assert_eq!(fixture.root.list_dir("tools").unwrap(), None);
        assert_eq!(fixture.root.file_len("logs/x.log").unwrap(), None);
        assert!(fixture.root.take_refusals().is_empty());
    }

    #[test]
    fn create_dir_all_applies_the_mode_to_what_it_creates() {
        let fixture = fixture();
        fixture
            .root
            .create_dir_all(".process-driver/run", 0o700)
            .unwrap();
        let mode = std::fs::metadata(fixture.workdir.join(".process-driver/run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(
            fixture.root.entry_kind(".process-driver").unwrap(),
            Some(EntryKind::Directory)
        );
    }

    #[test]
    fn remove_dir_all_unlinks_a_link_inside_without_following_it() {
        let fixture = fixture();
        std::fs::create_dir_all(fixture.workdir.join("tools/x/sub")).unwrap();
        std::fs::write(fixture.workdir.join("tools/x/sub/file"), b"x").unwrap();
        symlink(&fixture.outside, fixture.workdir.join("tools/x/escape")).unwrap();
        let before = fixture.outside_snapshot();
        fixture.root.remove_dir_all("tools/x").unwrap();
        assert!(!fixture.workdir.join("tools/x").exists());
        assert_eq!(fixture.outside_snapshot(), before);
    }

    #[test]
    fn overwrite_in_place_keeps_the_inode_and_refuses_a_link() {
        let fixture = fixture();
        fixture
            .root
            .overwrite_in_place(".mur-etc/passwd", b"one\n", 0o644)
            .unwrap();
        let inode = std::fs::metadata(fixture.workdir.join(".mur-etc/passwd"))
            .unwrap()
            .ino();
        fixture
            .root
            .overwrite_in_place(".mur-etc/passwd", b"two\n", 0o644)
            .unwrap();
        let meta = std::fs::metadata(fixture.workdir.join(".mur-etc/passwd")).unwrap();
        assert_eq!(meta.ino(), inode);
        assert_eq!(
            std::fs::read(fixture.workdir.join(".mur-etc/passwd")).unwrap(),
            b"two\n"
        );

        fixture.link_at_target(".mur-etc/group");
        let error = fixture
            .root
            .overwrite_in_place(".mur-etc/group", b"x", 0o644)
            .unwrap_err();
        assert_eq!(refusal_of(&error).reason, RefusalReason::Symlink);
        assert_eq!(std::fs::read(&fixture.sentinel).unwrap(), SENTINEL);
    }

    #[test]
    fn list_dir_reports_a_link_inside_as_a_symlink() {
        let fixture = fixture();
        std::fs::create_dir_all(fixture.workdir.join("tools/a")).unwrap();
        symlink(&fixture.outside, fixture.workdir.join("tools/b")).unwrap();
        assert_eq!(
            fixture.root.list_dir("tools").unwrap().unwrap(),
            vec![
                ("a".to_string(), EntryKind::Directory),
                ("b".to_string(), EntryKind::Symlink)
            ]
        );
    }

    #[test]
    fn a_refusal_serializes_with_the_wire_names() {
        let refusal = WorkdirRefusal {
            path: "task.md".into(),
            refused_at: "task.md".into(),
            operation: WorkdirOperation::CreateDir,
            reason: RefusalReason::NotARegularFile,
        };
        assert_eq!(
            serde_json::to_string(&refusal).unwrap(),
            r#"{"path":"task.md","refused_at":"task.md","operation":"create_dir","reason":"not_a_regular_file"}"#
        );
        for (op, wire) in [
            (WorkdirOperation::Write, "write"),
            (WorkdirOperation::Append, "append"),
            (WorkdirOperation::Read, "read"),
            (WorkdirOperation::CreateDir, "create_dir"),
            (WorkdirOperation::Remove, "remove"),
        ] {
            assert_eq!(serde_json::to_value(op).unwrap(), serde_json::json!(wire));
        }
        for (reason, wire) in [
            (RefusalReason::Symlink, "symlink"),
            (RefusalReason::HardLink, "hard_link"),
            (RefusalReason::NotARegularFile, "not_a_regular_file"),
            (RefusalReason::InvalidComponent, "invalid_component"),
        ] {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(wire)
            );
        }
    }
}
