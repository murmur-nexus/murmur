//! The one tree copy the runtime has: a delegated child's work folder snapshot
//! ([`crate::workdir_snapshot`]), an operator's `mur build-cache add` import and a session's
//! build cache seed ([`crate::build_cache`]) all walk through [`copy_tree`].
//!
//! # The walk
//!
//! Every read of the source and every write of the destination is relative to a held directory
//! descriptor, and no symbolic link is followed on either side:
//!
//! | Entry | Copied as |
//! |---|---|
//! | Regular file | Its content, its permission bits ([`Modes`]) and its modification time. Setuid, setgid and sticky are dropped. A reflink where the filesystem shares extents ([`Reflink`]), a byte copy otherwise. Never a hard link. |
//! | Directory | Recreated with its permission bits; its modification time is set after its contents. |
//! | Symbolic link | Recreated, never followed. An absolute target inside the source is re-pointed at the same place in the destination; every other target is copied verbatim. |
//! | FIFO, socket, device node | Not copied: [`SkipReason::Fifo`], [`SkipReason::Socket`], [`SkipReason::Device`]. |
//! | An entry the runtime cannot open | Skipped as [`SkipReason::Unreadable`], or the copy fails ([`Unreadable`]). |
//! | A top-level runtime write ([`crate::workdir_writes::runtime_writes`]), when asked | Not copied: [`SkipReason::RuntimeOwned`]. |
//! | A directory whose `(st_dev, st_ino)` is excluded, or is the destination itself | Not entered: [`SkipReason::RuntimeOwned`]. |
//!
//! # The ceiling
//!
//! Every regular file counts its size against [`CopyOptions::max_bytes`], reflinked or copied:
//! a reflinked file is as large to `stat` as a copied one, and every reader of a ceiling measures
//! apparent size. Crossing it stops the copy with [`CopyError::Ceiling`]; so does any write that
//! fails. The caller removes the partial destination.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileTimes, Permissions};
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::workdir_fs::{fd_identity, WorkdirRoot};

/// How many skipped entries a [`TreeCopy`] names; [`TreeCopy::skipped_count`] is the total.
pub const SKIPPED_SAMPLE_LIMIT: usize = 20;

/// Why an entry of the source was not copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    Fifo,
    Socket,
    /// A character or block device node.
    Device,
    /// The runtime could not open or read it.
    Unreadable,
    /// A path the runtime writes into a work folder, or one of its own directories.
    RuntimeOwned,
}

/// One entry of the source the copy left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedEntry {
    /// Relative to the source.
    pub path: String,
    pub reason: SkipReason,
}

/// What happens to an entry the runtime cannot open or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Left out and reported as [`SkipReason::Unreadable`]; the copy goes on.
    Skip,
    /// The copy stops with [`CopyError::Unreadable`]: a copy that must be whole or nothing.
    Fail,
}

/// The permission bits a copied entry gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modes {
    /// The source's user, group and other `rwx` bits.
    Keep,
    /// Owner-only: `0700` for a directory and for a file with its owner execute bit set, `0600`
    /// for every other file.
    Private,
}

/// Whether a regular file is first offered to the filesystem as a reflink (`FICLONE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reflink {
    /// Try a reflink, and copy the bytes when the filesystem refuses one. After the first
    /// refusal the rest of the walk copies bytes without asking again.
    Try,
    /// Always copy the bytes.
    Never,
}

/// What [`copy_tree`] is told beyond its two roots.
#[derive(Debug, Clone)]
pub struct CopyOptions {
    /// The most bytes of regular-file content the copy may hold.
    pub max_bytes: u64,
    /// `(st_dev, st_ino)` of directories never entered. The destination's own identity is always
    /// added.
    pub excluded: Vec<(u64, u64)>,
    /// Whether a top-level entry named by [`crate::workdir_writes::runtime_writes`] is left out.
    pub skip_runtime_writes: bool,
    pub unreadable: Unreadable,
    pub modes: Modes,
    pub reflink: Reflink,
}

/// What one [`copy_tree`] put into its destination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeCopy {
    /// Regular files copied, [`Self::reflinked`] plus [`Self::copied`].
    pub files: u64,
    /// Regular files whose extents the destination shares with the source.
    pub reflinked: u64,
    /// Regular files whose bytes were copied.
    pub copied: u64,
    /// Directories recreated beneath the destination, not counting the destination itself.
    pub dirs: u64,
    /// Symbolic links recreated, [`Self::repointed_symlinks`] among them.
    pub symlinks: u64,
    /// Links whose absolute target lay inside the source and now points into the destination.
    pub repointed_symlinks: u64,
    /// Bytes of regular-file content: what counted against the ceiling.
    pub bytes: u64,
    /// Every entry left out.
    pub skipped_count: u64,
    /// The first [`SKIPPED_SAMPLE_LIMIT`] entries left out, in walk order.
    pub skipped: Vec<SkippedEntry>,
}

/// Why a copy stopped. Either way the destination holds a partial copy the caller removes. Each
/// caller words these for its own reader.
#[derive(Debug)]
pub enum CopyError {
    /// The copied bytes would pass [`CopyOptions::max_bytes`] at `path`, relative to the source.
    Ceiling { max_bytes: u64, path: String },
    /// A read of the source's root or any write of the destination failed at `path`, a host path
    /// in the destination.
    Io {
        path: String,
        source: std::io::Error,
    },
    /// Under [`Unreadable::Fail`], the source entry at `path`, a host path, could not be opened
    /// or read.
    Unreadable {
        path: String,
        source: std::io::Error,
    },
}

/// Copies `source` into the empty `destination` by [the walk](self#the-walk), bounded by
/// `options.max_bytes`, and reports what it copied.
///
/// Opens nothing in `source` for writing. `destination`'s own mode and times are left as its
/// creator set them.
pub fn copy_tree(
    source: &WorkdirRoot,
    destination: &WorkdirRoot,
    options: &CopyOptions,
) -> Result<TreeCopy, CopyError> {
    let root_error = |path: &Path, error: crate::workdir_fs::WorkdirError| CopyError::Io {
        path: path.display().to_string(),
        source: error.into(),
    };
    let source_fd = source
        .dir_fd()
        .map_err(|error| root_error(source.path(), error))?;
    let destination_fd = destination
        .dir_fd()
        .map_err(|error| root_error(destination.path(), error))?;
    let mut excluded = options.excluded.clone();
    excluded.push(
        destination
            .identity()
            .map_err(|error| root_error(destination.path(), error))?,
    );
    let runtime_owned: Vec<String> = if options.skip_runtime_writes {
        crate::workdir_writes::runtime_writes()
            .into_iter()
            .map(|write| write.path)
            .collect()
    } else {
        Vec::new()
    };

    let mut walk = Walk {
        max_bytes: options.max_bytes,
        unreadable: options.unreadable,
        modes: options.modes,
        try_reflink: options.reflink == Reflink::Try,
        excluded,
        runtime_owned,
        source: source.path().to_path_buf(),
        source_prefixes: link_prefixes(source.path()),
        destination: destination.path().to_path_buf(),
        report: TreeCopy::default(),
    };
    walk.copy_dir(&source_fd, &destination_fd, "")?;
    Ok(walk.report)
}

/// The forms of `source` an absolute link target can name it by: as given, and resolved, so a
/// link written through either form is re-pointed.
fn link_prefixes(source: &Path) -> Vec<PathBuf> {
    let mut prefixes = vec![source.to_path_buf()];
    if let Ok(resolved) = std::fs::canonicalize(source) {
        if resolved != source {
            prefixes.push(resolved);
        }
    }
    prefixes
}

struct Walk {
    max_bytes: u64,
    unreadable: Unreadable,
    modes: Modes,
    /// Cleared by the first reflink the filesystem refuses.
    try_reflink: bool,
    excluded: Vec<(u64, u64)>,
    runtime_owned: Vec<String>,
    source: PathBuf,
    source_prefixes: Vec<PathBuf>,
    destination: PathBuf,
    report: TreeCopy,
}

const READ_DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// `O_NONBLOCK` so a source entry swapped for a FIFO after its `statat` cannot block the open.
const READ_FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

const CREATE_FILE: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// User, group and other `rwx`: what [`Modes::Keep`] keeps of a source mode.
const PERMISSION_BITS: u32 = 0o777;

impl Walk {
    /// An entry that could not be opened or read: skipped, or the end of the copy.
    fn unreadable(
        &mut self,
        rel: &str,
        source: impl Into<std::io::Error>,
    ) -> Result<(), CopyError> {
        match self.unreadable {
            Unreadable::Skip => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                Ok(())
            }
            Unreadable::Fail => Err(CopyError::Unreadable {
                path: self.source.join(rel).display().to_string(),
                source: source.into(),
            }),
        }
    }

    fn skip(&mut self, path: String, reason: SkipReason) {
        self.report.skipped_count += 1;
        if self.report.skipped.len() < SKIPPED_SAMPLE_LIMIT {
            self.report.skipped.push(SkippedEntry { path, reason });
        }
    }

    fn io(&self, rel: &str, source: impl Into<std::io::Error>) -> CopyError {
        CopyError::Io {
            path: self.destination.join(rel).display().to_string(),
            source: source.into(),
        }
    }

    fn file_mode(&self, source_mode: u32) -> u32 {
        match self.modes {
            Modes::Keep => source_mode & PERMISSION_BITS,
            Modes::Private if source_mode & 0o100 != 0 => 0o700,
            Modes::Private => 0o600,
        }
    }

    fn dir_mode(&self, source_mode: u32) -> u32 {
        match self.modes {
            Modes::Keep => source_mode & PERMISSION_BITS,
            Modes::Private => 0o700,
        }
    }

    /// Copies the entries of `source` into `destination`, `rel` being both directories' path
    /// relative to their roots.
    fn copy_dir(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        rel: &str,
    ) -> Result<(), CopyError> {
        let names = match list(source) {
            Ok(names) => names,
            Err(errno) if !rel.is_empty() => return self.unreadable(rel, errno),
            Err(errno) => return Err(self.io(rel, errno)),
        };
        for name in names {
            let child = if rel.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{rel}/{}", name.to_string_lossy())
            };
            if rel.is_empty()
                && self
                    .runtime_owned
                    .iter()
                    .any(|owned| name == owned.as_str())
            {
                self.skip(child, SkipReason::RuntimeOwned);
                continue;
            }
            let stat = match rustix::fs::statat(source, &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                // Removed since the listing: there is nothing left to copy or to report.
                Err(Errno::NOENT) => continue,
                Err(errno) => {
                    self.unreadable(&child, errno)?;
                    continue;
                }
            };
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => {
                    self.copy_file(source, destination, &name, &child, stat.st_size as u64)?
                }
                FileType::Directory => self.copy_subdir(source, destination, &name, &child)?,
                FileType::Symlink => self.copy_link(source, destination, &name, &child)?,
                FileType::Fifo => self.skip(child, SkipReason::Fifo),
                FileType::Socket => self.skip(child, SkipReason::Socket),
                FileType::CharacterDevice | FileType::BlockDevice => {
                    self.skip(child, SkipReason::Device)
                }
                FileType::Unknown => self.skip(child, SkipReason::Unreadable),
            }
        }
        Ok(())
    }

    fn check_ceiling(&self, rel: &str, more: u64) -> Result<(), CopyError> {
        if self.report.bytes.saturating_add(more) > self.max_bytes {
            return Err(CopyError::Ceiling {
                max_bytes: self.max_bytes,
                path: rel.to_string(),
            });
        }
        Ok(())
    }

    fn copy_file(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        name: &OsStr,
        rel: &str,
        size: u64,
    ) -> Result<(), CopyError> {
        self.check_ceiling(rel, size)?;
        let opened = match rustix::fs::openat(source, name, READ_FILE, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(()),
            Err(errno) => return self.unreadable(rel, errno),
        };
        let mut input = File::from(opened);
        let metadata = match input.metadata() {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => {
                return self.unreadable(
                    rel,
                    std::io::Error::other("no longer a regular file when opened"),
                )
            }
            Err(error) => return self.unreadable(rel, error),
        };
        let created = rustix::fs::openat(destination, name, CREATE_FILE, Mode::RUSR | Mode::WUSR)
            .map_err(|errno| self.io(rel, errno))?;
        let mut output = File::from(created);

        let reflinked = if self.try_reflink {
            match reflink(&output, &input) {
                Ok(()) => true,
                Err(errno) if reflink_refused(errno) => {
                    self.try_reflink = false;
                    false
                }
                Err(errno) => return Err(self.io(rel, errno)),
            }
        } else {
            false
        };
        let copied = if reflinked {
            // The clone took the whole file as it stands now, which may be more than `statat`
            // saw: the destination's own size is what counts.
            output
                .metadata()
                .map_err(|error| self.io(rel, error))?
                .len()
        } else {
            // One byte past what is left, so a file that grew since its `statat` is caught here
            // rather than copied past the ceiling.
            let room = self.max_bytes.saturating_sub(self.report.bytes);
            match std::io::copy(&mut (&mut input).take(room.saturating_add(1)), &mut output) {
                Ok(copied) => copied,
                // Readable by `statat` and open, unreadable by `read`.
                Err(error) if is_read_side(&error) => {
                    drop(output);
                    let _ = rustix::fs::unlinkat(destination, name, AtFlags::empty());
                    return self.unreadable(rel, error);
                }
                Err(error) => return Err(self.io(rel, error)),
            }
        };
        self.check_ceiling(rel, copied)?;
        output
            .set_permissions(Permissions::from_mode(
                self.file_mode(metadata.permissions().mode()),
            ))
            .map_err(|error| self.io(rel, error))?;
        if let Ok(modified) = metadata.modified() {
            output
                .set_times(FileTimes::new().set_modified(modified))
                .map_err(|error| self.io(rel, error))?;
        }
        self.report.bytes += copied;
        self.report.files += 1;
        if reflinked {
            self.report.reflinked += 1;
        } else {
            self.report.copied += 1;
        }
        Ok(())
    }

    fn copy_subdir(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        name: &OsStr,
        rel: &str,
    ) -> Result<(), CopyError> {
        let opened = match rustix::fs::openat(source, name, READ_DIR, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(()),
            Err(errno) => return self.unreadable(rel, errno),
        };
        match fd_identity(&opened) {
            Ok(identity) if self.excluded.contains(&identity) => {
                self.skip(rel.to_string(), SkipReason::RuntimeOwned);
                return Ok(());
            }
            Ok(_) => {}
            Err(errno) => return self.unreadable(rel, errno),
        }
        // Owner-only until its contents are in, so nothing else reads a half-copied directory and
        // a source directory without owner write still receives its entries.
        rustix::fs::mkdirat(destination, name, Mode::RWXU).map_err(|errno| self.io(rel, errno))?;
        let created = rustix::fs::openat(destination, name, READ_DIR, Mode::empty())
            .map_err(|errno| self.io(rel, errno))?;
        self.report.dirs += 1;
        self.copy_dir(&opened, &created, rel)?;

        let metadata = File::from(opened)
            .metadata()
            .map_err(|error| self.io(rel, error))?;
        let created = File::from(created);
        created
            .set_permissions(Permissions::from_mode(
                self.dir_mode(metadata.permissions().mode()),
            ))
            .map_err(|error| self.io(rel, error))?;
        if let Ok(modified) = metadata.modified() {
            created
                .set_times(FileTimes::new().set_modified(modified))
                .map_err(|error| self.io(rel, error))?;
        }
        Ok(())
    }

    fn copy_link(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        name: &OsStr,
        rel: &str,
    ) -> Result<(), CopyError> {
        let target = match rustix::fs::readlinkat(source, name, Vec::new()) {
            Ok(target) => target,
            Err(Errno::NOENT) => return Ok(()),
            Err(errno) => return self.unreadable(rel, errno),
        };
        let target = Path::new(OsStr::from_bytes(target.as_bytes()));
        let repointed = self.repoint(target);
        let written = repointed.as_deref().unwrap_or(target);
        rustix::fs::symlinkat(written, destination, name).map_err(|errno| self.io(rel, errno))?;
        self.report.symlinks += 1;
        if repointed.is_some() {
            self.report.repointed_symlinks += 1;
        }
        Ok(())
    }

    /// `target` re-pointed into the destination when it is an absolute path inside the source,
    /// else `None`.
    fn repoint(&self, target: &Path) -> Option<PathBuf> {
        if !target.is_absolute() {
            return None;
        }
        self.source_prefixes.iter().find_map(|prefix| {
            let rest = target.strip_prefix(prefix).ok()?;
            Some(if rest.as_os_str().is_empty() {
                self.destination.clone()
            } else {
                self.destination.join(rest)
            })
        })
    }
}

/// Shares `input`'s extents with the empty `output`: `FICLONE` on Linux.
#[cfg(target_os = "linux")]
fn reflink(output: &File, input: &File) -> rustix::io::Result<()> {
    rustix::fs::ioctl_ficlone(output, input)
}

/// No reflink call is made off Linux: every file is copied.
#[cfg(not(target_os = "linux"))]
fn reflink(_output: &File, _input: &File) -> rustix::io::Result<()> {
    Err(Errno::OPNOTSUPP)
}

/// Whether a failed reflink means "not here" — another filesystem, one that cannot share extents,
/// or a file it will not clone — so the file's bytes are copied instead. Anything else (`ENOSPC`,
/// `EIO`) stops the copy as a failed write would.
fn reflink_refused(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::XDEV
            | Errno::OPNOTSUPP
            | Errno::INVAL
            | Errno::NOTTY
            | Errno::BADF
            | Errno::NOSYS
            | Errno::PERM
    )
}

/// The names in the directory `dir`, sorted, `.` and `..` left out.
fn list(dir: &impl AsFd) -> rustix::io::Result<Vec<OsString>> {
    let mut listing = rustix::fs::Dir::read_from(dir)?;
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        let name = OsStr::from_bytes(entry?.file_name().to_bytes()).to_os_string();
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Whether a failed copy failed reading the source rather than writing the destination: `EACCES`
/// and `EPERM` are a source the runtime may not read; `ENOSPC`, `EIO` and the rest stop the copy.
fn is_read_side(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::ACCESS | Errno::PERM)
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::time::{Duration, SystemTime};

    /// A scratch folder beside the build's own output, which on a developer host shares its
    /// filesystem, rather than under `/tmp`, which is often a filesystem that cannot reflink.
    pub(crate) fn scratch() -> tempfile::TempDir {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tree-copy-scratch");
        std::fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix("t-")
            .tempdir_in(&base)
            .unwrap()
    }

    /// Whether `dir`'s filesystem shares extents on `FICLONE`.
    pub(crate) fn reflinks_in(dir: &Path) -> bool {
        let source = dir.join(".probe-source");
        std::fs::write(&source, b"probe").unwrap();
        let input = File::open(&source).unwrap();
        let output = File::create(dir.join(".probe-clone")).unwrap();
        let shared = reflink(&output, &input).is_ok();
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_file(dir.join(".probe-clone")).unwrap();
        shared
    }

    fn options(reflink: Reflink) -> CopyOptions {
        CopyOptions {
            max_bytes: u64::MAX,
            excluded: Vec::new(),
            skip_runtime_writes: false,
            unreadable: Unreadable::Fail,
            modes: Modes::Keep,
            reflink,
        }
    }

    fn write(path: &Path, bytes: &[u8], mode: u32, modified: SystemTime) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
    }

    /// A small target folder: a library, an executable build script, a nested fingerprint and a
    /// link pointing outside the tree.
    fn populate(source: &Path, outside: &Path) -> SystemTime {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        write(
            &source.join("CACHEDIR.TAG"),
            b"Signature: 8a477f597d28d172789f06886806bc55",
            0o644,
            when,
        );
        write(
            &source.join("debug/deps/libdep_a.rlib"),
            &vec![7u8; 70_000],
            0o644,
            when,
        );
        write(
            &source.join("debug/build/dep-a/build-script-build"),
            b"#!/bin/sh\n",
            0o755,
            when,
        );
        write(
            &source.join("debug/.fingerprint/dep-a/lib"),
            b"abc",
            0o600,
            when,
        );
        std::fs::write(outside.join("canary"), b"outside").unwrap();
        symlink(outside.join("canary"), source.join("debug/outside")).unwrap();
        when
    }

    fn open(path: &Path) -> WorkdirRoot {
        std::fs::create_dir_all(path).unwrap();
        WorkdirRoot::open(path).unwrap()
    }

    /// Both branches keep content, permission bits and modification times, recreate the link
    /// without following it, never hard-link, and count the same files and bytes; the reflink
    /// branch reports every file reflinked where the filesystem can, the forced fallback every
    /// file copied.
    #[test]
    fn tree_copy_keeps_content_modes_and_times_on_the_reflink_and_the_copy_branch() {
        let scratch = scratch();
        let can_reflink = reflinks_in(scratch.path());
        if !can_reflink {
            crate::runtime_err!(
                "[SKIP-HOST] {} cannot reflink: only the copy branch runs",
                scratch.path().display()
            );
        }
        let source = scratch.path().join("source");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let when = populate(&source, &outside);

        let mut reports = Vec::new();
        for (name, branch) in [("reflink", Reflink::Try), ("copy", Reflink::Never)] {
            let destination = scratch.path().join(name);
            let report = copy_tree(&open(&source), &open(&destination), &options(branch)).unwrap();
            for rel in [
                "CACHEDIR.TAG",
                "debug/deps/libdep_a.rlib",
                "debug/build/dep-a/build-script-build",
                "debug/.fingerprint/dep-a/lib",
            ] {
                let from = std::fs::metadata(source.join(rel)).unwrap();
                let to = std::fs::metadata(destination.join(rel)).unwrap();
                assert_eq!(
                    std::fs::read(source.join(rel)).unwrap(),
                    std::fs::read(destination.join(rel)).unwrap(),
                    "{name} {rel}"
                );
                assert_eq!(from.mode() & 0o777, to.mode() & 0o777, "{name} {rel}");
                assert_eq!(to.modified().unwrap(), when, "{name} {rel}");
                assert_eq!(to.nlink(), 1, "{name} {rel}");
                assert_ne!(from.ino(), to.ino(), "{name} {rel}");
            }
            let link = destination.join("debug/outside");
            assert!(std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read_link(&link).unwrap(), outside.join("canary"));
            assert_eq!(report.files, 4);
            assert_eq!(report.symlinks, 1);
            assert_eq!(report.reflinked + report.copied, report.files);
            match branch {
                Reflink::Never => assert_eq!(report.copied, 4),
                Reflink::Try if can_reflink => assert_eq!(report.reflinked, 4),
                Reflink::Try => assert_eq!(report.copied, 4),
            }
            reports.push(report);
        }
        assert_eq!(reports[0].bytes, reports[1].bytes);
        assert_eq!(reports[0].dirs, reports[1].dirs);

        // Writing a reflinked copy leaves the source as it was.
        std::fs::write(
            scratch.path().join("reflink/debug/deps/libdep_a.rlib"),
            b"junk",
        )
        .unwrap();
        assert_eq!(
            std::fs::read(source.join("debug/deps/libdep_a.rlib")).unwrap(),
            vec![7u8; 70_000]
        );
        assert_eq!(std::fs::read(outside.join("canary")).unwrap(), b"outside");
    }

    /// The ceiling counts a reflinked file by its size exactly as a copied one, and stops both at
    /// the same file.
    #[test]
    fn tree_copy_stops_at_the_ceiling_identically_on_both_branches() {
        let scratch = scratch();
        let source = scratch.path().join("source");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        populate(&source, &outside);
        let total = copy_tree(
            &open(&source),
            &open(&scratch.path().join("whole")),
            &options(Reflink::Never),
        )
        .unwrap()
        .bytes;
        let mut stops = Vec::new();
        for (name, branch) in [("reflink", Reflink::Try), ("copy", Reflink::Never)] {
            let error = copy_tree(
                &open(&source),
                &open(&scratch.path().join(name)),
                &CopyOptions {
                    max_bytes: total - 1,
                    ..options(branch)
                },
            )
            .unwrap_err();
            match error {
                CopyError::Ceiling { max_bytes, path } => {
                    assert_eq!(max_bytes, total - 1);
                    stops.push(path);
                }
                other => panic!("{name}: {other:?}"),
            }
            copy_tree(
                &open(&source),
                &open(&scratch.path().join(format!("{name}-exact"))),
                &CopyOptions {
                    max_bytes: total,
                    ..options(branch)
                },
            )
            .unwrap();
        }
        assert_eq!(stops[0], stops[1]);
    }

    /// [`Modes::Private`] narrows every directory and executable file to `0700` and every other
    /// file to `0600`.
    #[test]
    fn tree_copy_private_modes_are_owner_only() {
        let scratch = scratch();
        let source = scratch.path().join("source");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        populate(&source, &outside);
        std::fs::set_permissions(source.join("debug"), Permissions::from_mode(0o755)).unwrap();
        let destination = scratch.path().join("private");
        copy_tree(
            &open(&source),
            &open(&destination),
            &CopyOptions {
                modes: Modes::Private,
                ..options(Reflink::Try)
            },
        )
        .unwrap();
        let mode = |rel: &str| std::fs::metadata(destination.join(rel)).unwrap().mode() & 0o777;
        assert_eq!(mode("debug"), 0o700);
        assert_eq!(mode("debug/deps"), 0o700);
        assert_eq!(mode("debug/build/dep-a/build-script-build"), 0o700);
        assert_eq!(mode("debug/deps/libdep_a.rlib"), 0o600);
        assert_eq!(mode("CACHEDIR.TAG"), 0o600);
    }

    /// Under [`Unreadable::Fail`] a file the runtime cannot open ends the copy naming it; under
    /// [`Unreadable::Skip`] it is left out and the copy goes on.
    #[test]
    fn tree_copy_fails_or_skips_an_unreadable_file_as_asked() {
        if rustix::process::geteuid().is_root() {
            crate::runtime_err!("[SKIP-HOST] running as root: no file is unreadable");
            return;
        }
        let scratch = scratch();
        let source = scratch.path().join("source");
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        populate(&source, &outside);
        let secret = source.join("debug/deps/libdep_a.rlib");
        std::fs::set_permissions(&secret, Permissions::from_mode(0o000)).unwrap();
        let failed = copy_tree(
            &open(&source),
            &open(&scratch.path().join("fail")),
            &options(Reflink::Try),
        );
        let skipped = copy_tree(
            &open(&source),
            &open(&scratch.path().join("skip")),
            &CopyOptions {
                unreadable: Unreadable::Skip,
                ..options(Reflink::Try)
            },
        );
        std::fs::set_permissions(&secret, Permissions::from_mode(0o644)).unwrap();
        match failed {
            Err(CopyError::Unreadable { path, source }) => {
                assert_eq!(path, secret.display().to_string());
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("{other:?}"),
        }
        let skipped = skipped.unwrap();
        assert_eq!(skipped.files, 3);
        assert_eq!(skipped.skipped[0].reason, SkipReason::Unreadable);
    }
}
