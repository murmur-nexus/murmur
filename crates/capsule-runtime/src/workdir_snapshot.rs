//! Copying a delegating capsule's work folder into a delegated child's own, before the child
//! process starts.
//!
//! A parent lists a sub-capsule under `capabilities.spawn.workdir_snapshot`, and every delegation
//! to it starts the child in a work folder that already holds a copy of the parent's as it stood
//! at that moment. The parent's folder is the read-only side — the child is never handed any path
//! to it — and the child's own folder is the writable layer. A copy rather than a mount, because a
//! containment class is installed on the shell-subprocess tree only while WASM tools and the
//! runtime read the work folder from the host mount namespace: one copied folder is the same
//! folder to every reader, on every tier and platform.
//!
//! # The walk
//!
//! Every read of the source and every write of the destination is relative to a held directory
//! descriptor, and no symbolic link is followed on either side:
//!
//! | Entry | Copied as |
//! |---|---|
//! | Regular file | Its content, its `rwx` bits for user, group and other, and its modification time. Setuid, setgid and sticky are dropped. |
//! | Directory | Recreated with its `rwx` bits; its modification time is set after its contents. |
//! | Symbolic link | Recreated, never followed. An absolute target inside the source is re-pointed at the same place in the destination; every other target is copied verbatim. |
//! | FIFO, socket, device node | Not copied: [`SkipReason::Fifo`], [`SkipReason::Socket`], [`SkipReason::Device`]. |
//! | An entry the runtime cannot open | Not copied: [`SkipReason::Unreadable`]. |
//! | A top-level runtime write ([`crate::workdir_writes::runtime_writes`]) | Not copied: [`SkipReason::RuntimeOwned`]. |
//! | A directory whose `(st_dev, st_ino)` is excluded, or is the destination itself | Not entered: [`SkipReason::RuntimeOwned`]. |
//!
//! The exclusion by identity is what stops the copy recursing into itself when the parent's
//! runtime or records directory, and with it the child's new directory, lies inside the work
//! folder — an `advisory`-capped launch.
//!
//! # The ceiling
//!
//! The bytes of every copied regular file count against the child's own
//! `capabilities.resources.workdir_max_bytes`. Crossing it stops the copy with
//! [`SnapshotError::Ceiling`]; so does any write that fails. The caller removes the partial
//! destination and starts no process.

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

use crate::containment::FilesystemRestriction;
use crate::workdir_fs::{fd_identity, WorkdirRoot};

/// How many skipped entries a [`WorkdirSnapshot`] names; [`WorkdirSnapshot::skipped_count`] is
/// the total.
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
    /// Relative to the source work folder.
    pub path: String,
    pub reason: SkipReason,
}

/// What one copy of a parent's work folder put into a child's: the parent's
/// `delegation_start.workdir_snapshot`, and what the child is told through
/// `mur run --parent-workdir-snapshot`.
///
/// A report of what the parent did, never a grant: it names the parent's folder and carries no
/// token, credential or approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkdirSnapshot {
    /// The parent's work folder, as a host path.
    pub source: String,
    /// Regular files copied.
    pub files: u64,
    /// Directories recreated beneath the destination, not counting the destination itself.
    pub dirs: u64,
    /// Symbolic links recreated, [`Self::repointed_symlinks`] among them.
    pub symlinks: u64,
    /// Links whose absolute target lay inside the source and now points into the destination.
    pub repointed_symlinks: u64,
    /// Bytes of regular-file content copied: what counted against the ceiling.
    pub bytes: u64,
    /// Every entry left out.
    pub skipped_count: u64,
    /// The first [`SKIPPED_SAMPLE_LIMIT`] entries left out, in walk order.
    pub skipped: Vec<SkippedEntry>,
}

/// The child's `session_start.effective_grants.parent_workdir`: the snapshot it started with, and
/// what keeps its commands off the parent's real folder on the tier it applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParentWorkdirReport {
    #[serde(flatten)]
    pub snapshot: WorkdirSnapshot,
    /// The child's own filesystem restriction, from its applied tier: `absent` where the parent's
    /// folder is not in its composed root, `enforced` where Landlock refuses every open of it,
    /// `advisory` where nothing in the kernel does.
    pub restriction: FilesystemRestriction,
}

impl ParentWorkdirReport {
    /// The restriction in words, for `--explain-scope`'s `Parent work folder` block.
    #[must_use]
    pub fn restriction_sentence(&self) -> &'static str {
        match self.restriction {
            FilesystemRestriction::Absent => {
                "absent: the parent's work folder is not in this session's composed root"
            }
            FilesystemRestriction::Enforced => {
                "enforced: Landlock refuses every open of the parent's work folder"
            }
            FilesystemRestriction::Advisory => {
                "advisory: nothing in the kernel stops a command that names the parent's work \
                 folder by its absolute path; the runtime grants it nowhere and no copied link \
                 points into it"
            }
        }
    }
}

/// Why a copy stopped. Either way the destination holds a partial copy the caller removes.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The copied bytes would pass `max_bytes`.
    #[error(
        "the copy of the parent's work folder passes the child's \
         capabilities.resources.workdir_max_bytes ceiling of {max_bytes} bytes at {path}"
    )]
    Ceiling { max_bytes: u64, path: String },
    /// A read of the source's root or any write of the destination failed.
    #[error("copying the parent's work folder failed at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// What [`copy_workdir`] is told beyond its two roots.
#[derive(Debug, Clone, Default)]
pub struct SnapshotLimits {
    /// The child's effective `capabilities.resources.workdir_max_bytes`.
    pub max_bytes: u64,
    /// `(st_dev, st_ino)` of directories never entered: the parent session's runtime and records
    /// directories. The destination's own identity is always added.
    pub excluded: Vec<(u64, u64)>,
}

/// Copies `source` into the empty `destination` by [the walk](self#the-walk), bounded by
/// `limits.max_bytes`, and reports what it copied.
///
/// Opens nothing in `source` for writing. `destination`'s own mode and times are left as its
/// creator set them.
pub fn copy_workdir(
    source: &WorkdirRoot,
    destination: &WorkdirRoot,
    limits: &SnapshotLimits,
) -> Result<WorkdirSnapshot, SnapshotError> {
    let root_error = |path: &str, error: crate::workdir_fs::WorkdirError| SnapshotError::Io {
        path: path.to_string(),
        source: error.into(),
    };
    let source_fd = source
        .dir_fd()
        .map_err(|error| root_error(&source.path().display().to_string(), error))?;
    let destination_fd = destination
        .dir_fd()
        .map_err(|error| root_error(&destination.path().display().to_string(), error))?;
    let mut excluded = limits.excluded.clone();
    excluded.push(
        destination
            .identity()
            .map_err(|error| root_error(&destination.path().display().to_string(), error))?,
    );
    let runtime_owned: Vec<String> = crate::workdir_writes::runtime_writes()
        .into_iter()
        .map(|write| write.path)
        .collect();

    let mut walk = Walk {
        max_bytes: limits.max_bytes,
        excluded,
        runtime_owned,
        source_prefixes: link_prefixes(source.path()),
        destination: destination.path().to_path_buf(),
        snapshot: WorkdirSnapshot {
            source: source.path().display().to_string(),
            files: 0,
            dirs: 0,
            symlinks: 0,
            repointed_symlinks: 0,
            bytes: 0,
            skipped_count: 0,
            skipped: Vec::new(),
        },
    };
    walk.copy_dir(&source_fd, &destination_fd, "")?;
    Ok(walk.snapshot)
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
    excluded: Vec<(u64, u64)>,
    runtime_owned: Vec<String>,
    source_prefixes: Vec<PathBuf>,
    destination: PathBuf,
    snapshot: WorkdirSnapshot,
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

/// User, group and other `rwx`: what is kept of a source mode.
const PERMISSION_BITS: u32 = 0o777;

impl Walk {
    fn skip(&mut self, path: String, reason: SkipReason) {
        self.snapshot.skipped_count += 1;
        if self.snapshot.skipped.len() < SKIPPED_SAMPLE_LIMIT {
            self.snapshot.skipped.push(SkippedEntry { path, reason });
        }
    }

    fn io(&self, rel: &str, source: impl Into<std::io::Error>) -> SnapshotError {
        SnapshotError::Io {
            path: self.destination.join(rel).display().to_string(),
            source: source.into(),
        }
    }

    /// Copies the entries of `source` into `destination`, `rel` being both directories' path
    /// relative to their roots.
    fn copy_dir(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        rel: &str,
    ) -> Result<(), SnapshotError> {
        let names = match list(source) {
            Ok(names) => names,
            Err(_) if !rel.is_empty() => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
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
                Err(_) => {
                    self.skip(child, SkipReason::Unreadable);
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

    fn check_ceiling(&self, rel: &str, more: u64) -> Result<(), SnapshotError> {
        if self.snapshot.bytes.saturating_add(more) > self.max_bytes {
            return Err(SnapshotError::Ceiling {
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
    ) -> Result<(), SnapshotError> {
        self.check_ceiling(rel, size)?;
        let opened = match rustix::fs::openat(source, name, READ_FILE, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(()),
            Err(_) => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
        };
        let mut input = File::from(opened);
        let metadata = match input.metadata() {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            _ => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
        };
        let created = rustix::fs::openat(destination, name, CREATE_FILE, Mode::RUSR | Mode::WUSR)
            .map_err(|errno| self.io(rel, errno))?;
        let mut output = File::from(created);
        // One byte past what is left, so a file that grew since its `statat` is caught here
        // rather than copied past the ceiling.
        let room = self.max_bytes.saturating_sub(self.snapshot.bytes);
        let copied =
            match std::io::copy(&mut (&mut input).take(room.saturating_add(1)), &mut output) {
                Ok(copied) => copied,
                // Readable by `statat` and open, unreadable by `read`: the parent's own business.
                Err(error) if is_read_side(&error) => {
                    drop(output);
                    let _ = rustix::fs::unlinkat(destination, name, AtFlags::empty());
                    self.skip(rel.to_string(), SkipReason::Unreadable);
                    return Ok(());
                }
                Err(error) => return Err(self.io(rel, error)),
            };
        self.check_ceiling(rel, copied)?;
        output
            .set_permissions(Permissions::from_mode(
                metadata.permissions().mode() & PERMISSION_BITS,
            ))
            .map_err(|error| self.io(rel, error))?;
        if let Ok(modified) = metadata.modified() {
            output
                .set_times(FileTimes::new().set_modified(modified))
                .map_err(|error| self.io(rel, error))?;
        }
        self.snapshot.bytes += copied;
        self.snapshot.files += 1;
        Ok(())
    }

    fn copy_subdir(
        &mut self,
        source: &OwnedFd,
        destination: &OwnedFd,
        name: &OsStr,
        rel: &str,
    ) -> Result<(), SnapshotError> {
        let opened = match rustix::fs::openat(source, name, READ_DIR, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(()),
            Err(_) => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
        };
        match fd_identity(&opened) {
            Ok(identity) if self.excluded.contains(&identity) => {
                self.skip(rel.to_string(), SkipReason::RuntimeOwned);
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
        }
        // Owner-only until its contents are in, so nothing else reads a half-copied directory and
        // a source directory without owner write still receives its entries.
        rustix::fs::mkdirat(destination, name, Mode::RWXU).map_err(|errno| self.io(rel, errno))?;
        let created = rustix::fs::openat(destination, name, READ_DIR, Mode::empty())
            .map_err(|errno| self.io(rel, errno))?;
        self.snapshot.dirs += 1;
        self.copy_dir(&opened, &created, rel)?;

        let metadata = File::from(opened)
            .metadata()
            .map_err(|error| self.io(rel, error))?;
        let created = File::from(created);
        created
            .set_permissions(Permissions::from_mode(
                metadata.permissions().mode() & PERMISSION_BITS,
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
    ) -> Result<(), SnapshotError> {
        let target = match rustix::fs::readlinkat(source, name, Vec::new()) {
            Ok(target) => target,
            Err(Errno::NOENT) => return Ok(()),
            Err(_) => {
                self.skip(rel.to_string(), SkipReason::Unreadable);
                return Ok(());
            }
        };
        let target = Path::new(OsStr::from_bytes(target.as_bytes()));
        let repointed = self.repoint(target);
        let written = repointed.as_deref().unwrap_or(target);
        rustix::fs::symlinkat(written, destination, name).map_err(|errno| self.io(rel, errno))?;
        self.snapshot.symlinks += 1;
        if repointed.is_some() {
            self.snapshot.repointed_symlinks += 1;
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
/// and `EPERM` are a source the parent's runtime may not read, and skip the file; `ENOSPC`, `EIO`
/// and the rest stop the copy.
fn is_read_side(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::ACCESS | Errno::PERM)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::time::{Duration, SystemTime};

    struct Fixture {
        temp: tempfile::TempDir,
        source: PathBuf,
        destination: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("parent");
            let destination = temp.path().join("child");
            std::fs::create_dir(&source).unwrap();
            std::fs::create_dir(&destination).unwrap();
            Self {
                temp,
                source,
                destination,
            }
        }

        fn copy(&self, limits: &SnapshotLimits) -> Result<WorkdirSnapshot, SnapshotError> {
            copy_workdir(
                &WorkdirRoot::open(&self.source).unwrap(),
                &WorkdirRoot::open(&self.destination).unwrap(),
                limits,
            )
        }
    }

    fn unbounded() -> SnapshotLimits {
        SnapshotLimits {
            max_bytes: crate::resources::DEFAULT_WORKDIR_MAX_BYTES,
            excluded: Vec::new(),
        }
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo {}", path.display());
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn workdir_snapshot_copies_contents_permission_bits_and_times() {
        let fixture = Fixture::new();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        write(&fixture.source.join("notes.txt"), b"hello", 0o640);
        write(&fixture.source.join("check.sh"), b"#!/bin/sh\n", 0o755);
        File::options()
            .write(true)
            .open(fixture.source.join("notes.txt"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::create_dir_all(fixture.source.join("src/deep")).unwrap();
        write(&fixture.source.join("src/deep/lib.rs"), b"fn x() {}", 0o600);
        std::fs::set_permissions(
            fixture.source.join("src/deep"),
            Permissions::from_mode(0o750),
        )
        .unwrap();
        File::open(fixture.source.join("src"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let snapshot = fixture.copy(&unbounded()).unwrap();

        assert_eq!(snapshot.source, fixture.source.display().to_string());
        assert_eq!(
            (
                snapshot.files,
                snapshot.dirs,
                snapshot.symlinks,
                snapshot.bytes
            ),
            (3, 2, 0, 5 + 10 + 9)
        );
        assert_eq!(snapshot.skipped_count, 0);
        let copied = |rel: &str| fixture.destination.join(rel);
        assert_eq!(std::fs::read(copied("notes.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(copied("src/deep/lib.rs")).unwrap(),
            b"fn x() {}"
        );
        let mode = |rel: &str| std::fs::symlink_metadata(copied(rel)).unwrap().mode() & 0o7777;
        assert_eq!(mode("notes.txt"), 0o640);
        assert_eq!(mode("check.sh"), 0o755);
        assert_eq!(mode("src/deep"), 0o750);
        let modified = |rel: &str| std::fs::metadata(copied(rel)).unwrap().modified().unwrap();
        assert_eq!(modified("notes.txt"), old);
        assert_eq!(
            modified("src"),
            old,
            "a directory's time is set after its contents"
        );
    }

    #[test]
    fn workdir_snapshot_drops_setuid_setgid_and_sticky() {
        let fixture = Fixture::new();
        write(&fixture.source.join("tool"), b"x", 0o6755);
        std::fs::create_dir(fixture.source.join("shared")).unwrap();
        std::fs::set_permissions(
            fixture.source.join("shared"),
            Permissions::from_mode(0o1777),
        )
        .unwrap();

        fixture.copy(&unbounded()).unwrap();

        let mode = |rel: &str| {
            std::fs::symlink_metadata(fixture.destination.join(rel))
                .unwrap()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("tool"), 0o755);
        assert_eq!(mode("shared"), 0o777);
    }

    #[test]
    fn workdir_snapshot_recreates_links_without_following_them_and_repoints_into_the_copy() {
        let fixture = Fixture::new();
        let outside = fixture.temp.path().join("outside.txt");
        write(&outside, b"outside", 0o644);
        write(&fixture.source.join("real.txt"), b"real", 0o644);
        symlink("real.txt", fixture.source.join("relative")).unwrap();
        symlink(&outside, fixture.source.join("absolute-outside")).unwrap();
        symlink(
            fixture.source.join("real.txt"),
            fixture.source.join("absolute-inside"),
        )
        .unwrap();
        symlink(&fixture.source, fixture.source.join("root-link")).unwrap();
        symlink("/nonexistent/dangling", fixture.source.join("dangling")).unwrap();

        let snapshot = fixture.copy(&unbounded()).unwrap();

        assert_eq!(snapshot.files, 1, "no link was followed into a copied file");
        assert_eq!(snapshot.bytes, 4);
        assert_eq!(snapshot.symlinks, 5);
        assert_eq!(snapshot.repointed_symlinks, 2);
        let target = |rel: &str| std::fs::read_link(fixture.destination.join(rel)).unwrap();
        assert_eq!(target("relative"), PathBuf::from("real.txt"));
        assert_eq!(target("absolute-outside"), outside);
        assert_eq!(target("dangling"), PathBuf::from("/nonexistent/dangling"));
        assert_eq!(
            target("absolute-inside"),
            fixture.destination.join("real.txt")
        );
        assert_eq!(target("root-link"), fixture.destination);
        assert_eq!(
            std::fs::read(fixture.destination.join("absolute-inside")).unwrap(),
            b"real"
        );
    }

    #[test]
    fn workdir_snapshot_skips_a_fifo_without_blocking_and_an_unreadable_file() {
        let fixture = Fixture::new();
        mkfifo(&fixture.source.join("pipe"));
        write(&fixture.source.join("secret"), b"nope", 0o000);
        write(&fixture.source.join("plain"), b"ok", 0o644);
        let readable_anyway = File::open(fixture.source.join("secret")).is_ok();

        let snapshot = fixture.copy(&unbounded()).unwrap();

        assert!(snapshot.skipped.contains(&SkippedEntry {
            path: "pipe".to_string(),
            reason: SkipReason::Fifo,
        }));
        assert!(!fixture.destination.join("pipe").exists());
        if !readable_anyway {
            assert!(snapshot.skipped.contains(&SkippedEntry {
                path: "secret".to_string(),
                reason: SkipReason::Unreadable,
            }));
            assert!(!fixture.destination.join("secret").exists());
            assert_eq!(snapshot.skipped_count, 2);
        }
        assert_eq!(
            std::fs::read(fixture.destination.join("plain")).unwrap(),
            b"ok"
        );
    }

    #[test]
    fn workdir_snapshot_skips_top_level_runtime_writes_only() {
        let fixture = Fixture::new();
        for dir in ["out", "peer-in", "nested/out"] {
            std::fs::create_dir_all(fixture.source.join(dir)).unwrap();
            write(&fixture.source.join(dir).join("f"), b"x", 0o644);
        }

        let snapshot = fixture.copy(&unbounded()).unwrap();

        assert_eq!(
            snapshot.skipped,
            vec![
                SkippedEntry {
                    path: "out".to_string(),
                    reason: SkipReason::RuntimeOwned,
                },
                SkippedEntry {
                    path: "peer-in".to_string(),
                    reason: SkipReason::RuntimeOwned,
                },
            ]
        );
        assert!(!fixture.destination.join("out").exists());
        assert!(!fixture.destination.join("peer-in").exists());
        assert!(fixture.destination.join("nested/out/f").is_file());
    }

    #[test]
    fn workdir_snapshot_does_not_enter_an_excluded_identity_or_its_own_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("work");
        let records = source.join(".murmur/records");
        let destination = records.join("children/reviewer-0");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(source.join(".murmur/runtime")).unwrap();
        write(&records.join("trace.jsonl"), b"{}", 0o644);
        write(&source.join(".murmur/runtime/MURMUR.md"), b"#", 0o644);
        write(&source.join("code.rs"), b"fn", 0o644);
        let runtime = WorkdirRoot::open(&source.join(".murmur/runtime")).unwrap();

        let snapshot = copy_workdir(
            &WorkdirRoot::open(&source).unwrap(),
            &WorkdirRoot::open(&destination).unwrap(),
            &SnapshotLimits {
                max_bytes: 1 << 20,
                excluded: vec![runtime.identity().unwrap()],
            },
        )
        .unwrap();

        // `code.rs`, and the records directory's `trace.jsonl`: only the runtime directory is
        // excluded here, so the walk reaches the destination and has to skip it by itself.
        assert_eq!(snapshot.files, 2, "{snapshot:?}");
        assert!(destination.join("code.rs").is_file());
        assert!(!destination.join(".murmur/runtime").exists());
        assert!(snapshot.skipped.contains(&SkippedEntry {
            path: ".murmur/runtime".to_string(),
            reason: SkipReason::RuntimeOwned,
        }));
        assert!(snapshot.skipped.contains(&SkippedEntry {
            path: ".murmur/records/children/reviewer-0".to_string(),
            reason: SkipReason::RuntimeOwned,
        }));
        assert!(!destination
            .join(".murmur/records/children/reviewer-0")
            .exists());
    }

    #[test]
    fn workdir_snapshot_stops_at_the_ceiling() {
        let fixture = Fixture::new();
        write(&fixture.source.join("big.bin"), &[7u8; 4096], 0o644);

        let error = fixture
            .copy(&SnapshotLimits {
                max_bytes: 1024,
                excluded: Vec::new(),
            })
            .unwrap_err();

        assert!(
            matches!(&error, SnapshotError::Ceiling { max_bytes: 1024, path } if path == "big.bin"),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("capabilities.resources.workdir_max_bytes")
                && message.contains("1024"),
            "{message}"
        );

        let fresh = Fixture::new();
        write(&fresh.source.join("exact.bin"), &[1u8; 1024], 0o644);
        let snapshot = fresh
            .copy(&SnapshotLimits {
                max_bytes: 1024,
                excluded: Vec::new(),
            })
            .unwrap();
        assert_eq!(snapshot.bytes, 1024, "exactly the ceiling fits");
    }

    #[test]
    fn workdir_snapshot_keeps_at_most_twenty_samples_and_counts_every_skip() {
        let fixture = Fixture::new();
        for index in 0..25 {
            mkfifo(&fixture.source.join(format!("pipe-{index:02}")));
        }

        let snapshot = fixture.copy(&unbounded()).unwrap();

        assert_eq!(snapshot.skipped_count, 25);
        assert_eq!(snapshot.skipped.len(), SKIPPED_SAMPLE_LIMIT);
        assert_eq!(snapshot.skipped[0].path, "pipe-00");
    }

    #[test]
    fn workdir_snapshot_serializes_the_documented_keys() {
        let snapshot = WorkdirSnapshot {
            source: "/home/you/project".to_string(),
            files: 12,
            dirs: 3,
            symlinks: 1,
            repointed_symlinks: 0,
            bytes: 40960,
            skipped_count: 1,
            skipped: vec![SkippedEntry {
                path: "run/pipe".to_string(),
                reason: SkipReason::Fifo,
            }],
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(
            json,
            r#"{"source":"/home/you/project","files":12,"dirs":3,"symlinks":1,"repointed_symlinks":0,"bytes":40960,"skipped_count":1,"skipped":[{"path":"run/pipe","reason":"fifo"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<WorkdirSnapshot>(&json).unwrap(),
            snapshot
        );
        let report = serde_json::to_value(ParentWorkdirReport {
            snapshot,
            restriction: FilesystemRestriction::Enforced,
        })
        .unwrap();
        assert_eq!(report["restriction"], "enforced");
        assert_eq!(report["files"], 12);
        for reason in [
            (SkipReason::Socket, "socket"),
            (SkipReason::Device, "device"),
            (SkipReason::Unreadable, "unreadable"),
            (SkipReason::RuntimeOwned, "runtime-owned"),
        ] {
            assert_eq!(serde_json::to_value(reason.0).unwrap(), reason.1);
        }
    }

    #[test]
    fn workdir_snapshot_leaves_the_source_unchanged() {
        let fixture = Fixture::new();
        write(&fixture.source.join("a"), b"a", 0o444);
        std::fs::create_dir(fixture.source.join("ro")).unwrap();
        write(&fixture.source.join("ro/b"), b"b", 0o644);
        std::fs::set_permissions(fixture.source.join("ro"), Permissions::from_mode(0o555)).unwrap();
        let before = digest(&fixture.source);

        fixture.copy(&unbounded()).unwrap();
        // The copy of a read-only directory is still removable by its owner's runtime.
        WorkdirRoot::open(fixture.destination.parent().unwrap())
            .unwrap()
            .remove_dir_all("child")
            .unwrap();

        assert_eq!(digest(&fixture.source), before);
        std::fs::set_permissions(fixture.source.join("ro"), Permissions::from_mode(0o755)).unwrap();
    }

    fn digest(root: &Path) -> Vec<(String, u32, Vec<u8>)> {
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                let contents = if metadata.is_file() {
                    std::fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                if metadata.is_dir() {
                    stack.push(path.clone());
                }
                entries.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    metadata.mode(),
                    contents,
                ));
            }
        }
        entries.sort();
        entries
    }
}
