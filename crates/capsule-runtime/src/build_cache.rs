//! The build cache store, `~/.murmur/build-cache/`: Cargo target folders an operator built on the
//! host and imported with `mur build-cache add`, and the private copy of one a session starts
//! with.
//!
//! ```text
//! ~/.murmur/build-cache/
//!   <sha256>/              keyed by the SHA-256 of the Cargo.lock the target was built from
//!     entry.json           EntryRecord
//!     target/              the imported target folder
//!   .partial-<random>/     an import in flight, or an entry being replaced; never read
//! ```
//!
//! Every directory is `0700`; every file `0700` when it is executable and `0600` otherwise. An
//! entry is assembled in a `.partial-*` folder and moved into place by one `rename`, so a reader
//! sees a whole entry or none.
//!
//! The store is written by `mur build-cache add` alone ([`add`]); nothing a capsule produced is
//! ever copied into it. Staging only reads it ([`seed_session`]): when the accessible work
//! folder's root holds a `Cargo.lock` whose bytes hash to an entry's key, the entry's `target/` is
//! copied — reflinked where the filesystem can — into the session's own home folder at
//! [`SEED_TARGET_REL`], and every command of the session gets [`CARGO_TARGET_DIR_ENV`] naming it.
//! Each session writes only its own copy, and the copy is removed when the session ends
//! ([`SeededTarget`]).

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize};

use crate::member_call::elapsed_ms;
use crate::murmur_home::PartialDir;
use crate::tree_copy::{CopyError, CopyOptions, Modes, Reflink, Unreadable};
use crate::workdir_fs::{EntryKind, WorkdirRoot};

/// The store's name under `~/.murmur`.
pub const BUILD_CACHE_DIR: &str = "build-cache";

/// An entry's record of what was imported.
pub const ENTRY_FILE: &str = "entry.json";

/// The folder of an entry holding the imported target folder.
pub const ENTRY_TARGET_DIR: &str = "target";

pub use crate::murmur_home::PARTIAL_PREFIX;

/// The file at a work folder's root whose bytes key the lookup.
pub const LOCKFILE_NAME: &str = "Cargo.lock";

/// The largest `Cargo.lock` the lookup reads; a larger one finds no entry.
pub const MAX_LOCKFILE_BYTES: u64 = 16 * 1024 * 1024;

/// Where in the session's home folder the seed is copied to.
pub const SEED_TARGET_REL: &str = ".cache/cargo-target";

/// The variable every command of a seeded session reads the seed's path from.
pub const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// The files Cargo writes into every target folder it uses; `add` takes a source holding either.
pub const TARGET_MARKERS: [&str; 2] = ["CACHEDIR.TAG", ".rustc_info.json"];

/// The parent of the seed in the session's home folder.
pub(crate) const SEED_CACHE_DIR: &str = ".cache";

/// The seed's own name beneath [`SEED_CACHE_DIR`].
pub(crate) const SEED_TARGET_DIR: &str = "cargo-target";

/// The largest `.rustc_info.json` read for [`EntryRecord::rustc`].
const MAX_RUSTC_INFO_BYTES: u64 = 1024 * 1024;

/// The largest `entry.json` read.
const MAX_ENTRY_RECORD_BYTES: u64 = 1024 * 1024;

/// `~/.murmur/build-cache`, whether or not it exists. Creates nothing.
pub fn build_cache_store_root() -> Result<PathBuf, String> {
    Ok(crate::state_store::murmur_home_dir()?.join(BUILD_CACHE_DIR))
}

/// The key of a `Cargo.lock`: the lowercase hex SHA-256 of its bytes.
#[must_use]
pub fn lockfile_key(lockfile: &[u8]) -> String {
    murmur_artifact::sha256_hex(lockfile)
}

/// `entry.json`: where an entry was imported from and what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRecord {
    /// The SHA-256 of the `Cargo.lock` the entry is keyed by, its folder name.
    pub lockfile_sha256: String,
    /// When the entry was committed, RFC 3339 UTC.
    pub added_at: String,
    /// The target folder imported, as an absolute host path.
    pub source: String,
    /// Regular files under `target/`.
    pub files: u64,
    /// Bytes of regular-file content under `target/`.
    pub bytes: u64,
    /// The first line of the `rustc -vV` output Cargo recorded in the source's
    /// `.rustc_info.json`, or `None` when it recorded none.
    pub rustc: Option<String>,
}

/// Why `mur build-cache add` refused or failed. Nothing is left in the store on any of them.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("the lockfile {} does not exist", path.display())]
    LockfileMissing { path: PathBuf },
    #[error("the lockfile {} is not a regular file", path.display())]
    LockfileNotRegular { path: PathBuf },
    #[error("the target folder {} does not exist", path.display())]
    SourceMissing { path: PathBuf },
    #[error(
        "{} is not a Cargo target folder: it {reason}",
        path.display()
    )]
    SourceNotTarget { path: PathBuf, reason: String },
    #[error(
        "{} is inside {}, where every session's records and home folder live, so it is never \
         imported",
        path.display(),
        murmur_home.display()
    )]
    SourceInsideMurmurHome { path: PathBuf, murmur_home: PathBuf },
    /// A file of the source could not be read; an import is whole or nothing.
    #[error("cannot read {path}: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// What [`add`] stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedEntry {
    /// `<store_root>/<sha256>`.
    pub entry: PathBuf,
    pub record: EntryRecord,
    /// Files the import reflinked rather than copied.
    pub reflinked: u64,
}

/// Imports the Cargo target folder `source`, built from `lockfile`, as the store's entry for that
/// lockfile, replacing any entry it already has.
///
/// Refuses a `lockfile` that is missing or not a regular file, a `source` that is missing, is not
/// a directory, holds none of [`TARGET_MARKERS`] or lies inside `murmur_home`, before anything is
/// written. The copy keeps every file's content and modification time, narrows modes to `0700`
/// and `0600`, follows no link inside `source` and fails on any file it cannot read.
pub fn add(
    store_root: &Path,
    murmur_home: &Path,
    lockfile: &Path,
    source: &Path,
) -> Result<AddedEntry, AddError> {
    let io = |path: &Path, source: std::io::Error| AddError::Io {
        path: path.display().to_string(),
        source,
    };
    let lockfile_bytes = match std::fs::metadata(lockfile) {
        Ok(metadata) if metadata.is_file() => {
            std::fs::read(lockfile).map_err(|e| io(lockfile, e))?
        }
        Ok(_) => {
            return Err(AddError::LockfileNotRegular {
                path: lockfile.to_path_buf(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AddError::LockfileMissing {
                path: lockfile.to_path_buf(),
            })
        }
        Err(error) => return Err(io(lockfile, error)),
    };
    let key = lockfile_key(&lockfile_bytes);

    let resolved = match std::fs::canonicalize(source) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AddError::SourceMissing {
                path: source.to_path_buf(),
            })
        }
        Err(error) => return Err(io(source, error)),
    };
    let resolved_home = std::fs::canonicalize(murmur_home).unwrap_or(murmur_home.to_path_buf());
    if resolved.starts_with(&resolved_home) || source.starts_with(murmur_home) {
        return Err(AddError::SourceInsideMurmurHome {
            path: resolved,
            murmur_home: murmur_home.to_path_buf(),
        });
    }
    let metadata = std::fs::metadata(&resolved).map_err(|e| io(&resolved, e))?;
    if !metadata.is_dir() {
        return Err(AddError::SourceNotTarget {
            path: resolved,
            reason: "is not a directory".to_string(),
        });
    }
    let marked = TARGET_MARKERS.iter().any(|marker| {
        std::fs::symlink_metadata(resolved.join(marker)).is_ok_and(|meta| meta.is_file())
    });
    if !marked {
        return Err(AddError::SourceNotTarget {
            path: resolved,
            reason: "holds neither CACHEDIR.TAG nor .rustc_info.json, one of which Cargo writes \
                     into every target folder it uses"
                .to_string(),
        });
    }

    crate::murmur_home::ensure_murmur_home_at(murmur_home)
        .map_err(|reason| io(murmur_home, std::io::Error::other(reason)))?;
    crate::murmur_home::hold_private_dir(store_root)
        .map_err(|reason| io(store_root, std::io::Error::other(reason)))?;

    let partial = PartialDir::create(store_root).map_err(|e| io(store_root, e))?;
    let target = partial.path().join(ENTRY_TARGET_DIR);
    crate::murmur_home::hold_private_dir(&target)
        .map_err(|reason| io(&target, std::io::Error::other(reason)))?;
    let held = |path: &Path| {
        WorkdirRoot::open(path).map_err(|error| io(path, std::io::Error::from(error)))
    };
    let copied = crate::tree_copy::copy_tree(
        &held(&resolved)?,
        &held(&target)?,
        &CopyOptions {
            max_bytes: u64::MAX,
            excluded: Vec::new(),
            skip_runtime_writes: false,
            unreadable: Unreadable::Fail,
            modes: Modes::Private,
            reflink: Reflink::Try,
        },
    )
    .map_err(|error| match error {
        CopyError::Unreadable { path, source } => AddError::Unreadable { path, source },
        CopyError::Io { path, source } => AddError::Io { path, source },
        CopyError::Ceiling { path, .. } => AddError::Io {
            path,
            source: std::io::Error::other("larger than any store entry can be"),
        },
    })?;

    let record = EntryRecord {
        lockfile_sha256: key.clone(),
        added_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        source: resolved.display().to_string(),
        files: copied.files,
        bytes: copied.bytes,
        rustc: rustc_of_target(&resolved),
    };
    let record_path = partial.path().join(ENTRY_FILE);
    write_record(&record_path, &record).map_err(|e| io(&record_path, e))?;

    let entry = store_root.join(&key);
    commit(partial, store_root, &entry).map_err(|e| io(&entry, e))?;
    Ok(AddedEntry {
        entry,
        record,
        reflinked: copied.reflinked,
    })
}

/// Moves the assembled `partial` to `entry`. An entry already there is first renamed to a
/// `.partial-*` name, then removed once the new one is in place, or put back if it is not.
fn commit(mut partial: PartialDir, store_root: &Path, entry: &Path) -> std::io::Result<()> {
    let replaced = match std::fs::symlink_metadata(entry) {
        Ok(_) => {
            let aside = crate::murmur_home::partial_name(store_root)?;
            std::fs::rename(entry, &aside)?;
            Some(aside)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Err(error) = std::fs::rename(partial.path(), entry) {
        if let Some(aside) = &replaced {
            let _ = std::fs::rename(aside, entry);
        }
        return Err(error);
    }
    partial.keep();
    if let Some(aside) = replaced {
        std::fs::remove_dir_all(&aside)?;
    }
    Ok(())
}

fn write_record(path: &Path, record: &EntryRecord) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(crate::murmur_home::PRIVATE_FILE_MODE)
        .open(path)?;
    file.write_all(&bytes)
}

/// The first line of the `rustc -vV` output Cargo recorded in `target/.rustc_info.json`, e.g.
/// `rustc 1.95.0 (59807616e 2026-04-14)`, or `None` when there is no such file or output.
pub fn rustc_of_target(target: &Path) -> Option<String> {
    let file = std::fs::File::open(target.join(".rustc_info.json")).ok()?;
    let mut text = String::new();
    file.take(MAX_RUSTC_INFO_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    let info: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut lines: Vec<String> = info
        .get("outputs")?
        .as_object()?
        .values()
        .filter_map(|output| output.get("stdout")?.as_str())
        .filter(|stdout| stdout.starts_with("rustc ") && stdout.contains("\nrelease: "))
        .filter_map(|stdout| stdout.lines().next().map(str::to_string))
        .collect();
    lines.sort();
    lines.into_iter().next()
}

/// One entry of the store as [`list`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    /// The entry's folder name, the lockfile SHA-256.
    pub key: String,
    pub entry: PathBuf,
    /// Its `entry.json`, or why it could not be read.
    pub record: Result<EntryRecord, String>,
}

/// Every entry in `store_root`, sorted by key; `[]` when the store does not exist. Folders being
/// assembled or replaced (`.partial-*`) are left out. Reads only.
pub fn list(store_root: &Path) -> std::io::Result<Vec<ListedEntry>> {
    let entries = match std::fs::read_dir(store_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut listed = Vec::new();
    for dir_entry in entries {
        let dir_entry = dir_entry?;
        let key = dir_entry.file_name().to_string_lossy().into_owned();
        if key.starts_with('.')
            || !dir_entry
                .file_type()
                .is_ok_and(|file_type| file_type.is_dir())
        {
            continue;
        }
        let entry = dir_entry.path();
        let record = read_entry_record(&entry);
        listed.push(ListedEntry { key, entry, record });
    }
    listed.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(listed)
}

/// The `entry.json` of the entry folder `entry`.
pub fn read_entry_record(entry: &Path) -> Result<EntryRecord, String> {
    let path = entry.join(ENTRY_FILE);
    let file =
        std::fs::File::open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_ENTRY_RECORD_BYTES)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Whether a session was given a seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SeedOutcome {
    /// The entry was copied whole and `CARGO_TARGET_DIR` names the copy.
    Seeded,
    /// A matching entry could not be used (`W-RUN-014`); the session builds without it.
    Skipped,
}

/// `session_start.build_cache` when a store entry matched the work folder's `Cargo.lock`; the key
/// is `null` when none did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildCacheReport {
    pub outcome: SeedOutcome,
    /// Why a matching entry was not seeded; `None` when it was.
    pub reason: Option<String>,
    /// The SHA-256 of the work folder's `Cargo.lock`, the entry's key.
    pub lockfile_sha256: String,
    /// The entry folder, as a host path.
    pub entry: String,
    /// The seed, `<records_dir>/home/.cache/cargo-target`, and the value of `CARGO_TARGET_DIR`;
    /// `None` when nothing was seeded.
    pub target_dir: Option<String>,
    /// Regular files seeded, [`Self::reflinked`] plus [`Self::copied`]; `0` when skipped.
    pub files: u64,
    /// Bytes of regular-file content seeded; `0` when skipped.
    pub bytes: u64,
    /// Files whose extents the seed shares with the store.
    pub reflinked: u64,
    /// Files whose bytes were copied.
    pub copied: u64,
    /// From the start of the lookup to the end of the copy, or to the skip.
    pub duration_ms: u64,
    /// The entry's [`EntryRecord::rustc`], `None` when it names none or its record is unreadable.
    pub rustc: Option<String>,
}

/// A session's seed, removed when the value is dropped: at the end of `launch`, or with a staged
/// session that is never launched.
///
/// Held through its own descriptor of `home/.cache`, so the removal resolves nothing by path and
/// latches no refusal on the session: a seed a command replaced with a link is unlinked, and a
/// link inside it is unlinked, never followed.
pub struct SeededTarget {
    cache_root: WorkdirRoot,
    path: PathBuf,
}

impl std::fmt::Debug for SeededTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeededTarget")
            .field("path", &self.path)
            .finish()
    }
}

impl SeededTarget {
    /// The seed's absolute path, the value of `CARGO_TARGET_DIR`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the seed now. Removing it again is a no-op.
    pub fn remove(&self) {
        let removed = match self.cache_root.entry_kind(SEED_TARGET_DIR) {
            Ok(None) => return,
            Ok(Some(EntryKind::Directory)) => self
                .cache_root
                .remove_dir_all(SEED_TARGET_DIR)
                .map_err(std::io::Error::from),
            Ok(Some(_)) => self
                .cache_root
                .dir_fd()
                .map_err(std::io::Error::from)
                .and_then(|dir| {
                    rustix::fs::unlinkat(&dir, SEED_TARGET_DIR, rustix::fs::AtFlags::empty())
                        .map_err(std::io::Error::from)
                }),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = removed {
            crate::runtime_err!(
                "[capsule-runtime] warning: failed to remove the build cache seed {}: {error}",
                self.path.display()
            );
        }
    }
}

impl Drop for SeededTarget {
    fn drop(&mut self) {
        self.remove();
    }
}

/// What staging tells [`seed_session`] beyond the two roots.
#[derive(Debug, Clone)]
pub struct SeedLimits<'a> {
    /// The session's effective `capabilities.resources.workdir_max_bytes`.
    pub max_bytes: u64,
    /// The folders measured with the seed against `max_bytes`: the work folder and the session's
    /// `tmp/` and `home/`.
    pub measured: Vec<&'a Path>,
    /// Whether a file is offered to the filesystem as a reflink first.
    pub reflink: Reflink,
}

/// The seed staging made, or why it made none.
#[derive(Debug)]
pub struct SessionSeed {
    pub report: BuildCacheReport,
    /// The seed, `None` when [`BuildCacheReport::outcome`] is [`SeedOutcome::Skipped`].
    pub seeded: Option<SeededTarget>,
}

/// Looks up the store entry for the `Cargo.lock` at `workdir`'s root and copies its `target/`
/// into `session_home`'s [`SEED_TARGET_REL`].
///
/// `None` when nothing matches: no regular `Cargo.lock` at the root (a link is not followed), one
/// larger than [`MAX_LOCKFILE_BYTES`], or no entry for its key. Otherwise the copy is all or
/// nothing: any failure removes what was copied, and the report says why
/// ([`SeedOutcome::Skipped`]). Never fails the launch, and writes nothing in `workdir` or the
/// store.
pub fn seed_session(
    workdir: &WorkdirRoot,
    session_home_root: &WorkdirRoot,
    store_root: &Path,
    limits: &SeedLimits<'_>,
) -> Option<SessionSeed> {
    let lockfile = read_root_lockfile(workdir)?;
    let started = Instant::now();
    let key = lockfile_key(&lockfile);
    let entry = store_root.join(&key);
    std::fs::symlink_metadata(&entry).ok()?;

    let record = read_entry_record(&entry);
    let mut report = BuildCacheReport {
        outcome: SeedOutcome::Skipped,
        reason: None,
        lockfile_sha256: key,
        entry: entry.display().to_string(),
        target_dir: None,
        files: 0,
        bytes: 0,
        reflinked: 0,
        copied: 0,
        duration_ms: 0,
        rustc: record.as_ref().ok().and_then(|record| record.rustc.clone()),
    };
    let skipped = |mut report: BuildCacheReport, reason: String| {
        report.reason = Some(reason);
        report.duration_ms = elapsed_ms(started);
        Some(SessionSeed {
            report,
            seeded: None,
        })
    };
    let record = match record {
        Ok(record) => record,
        Err(reason) => return skipped(report, format!("its record cannot be read: {reason}")),
    };
    let used = limits.measured.iter().fold(0u64, |sum, dir| {
        sum.saturating_add(crate::resources::directory_size_bytes(dir))
    });
    if record.bytes.saturating_add(used) > limits.max_bytes {
        return skipped(
            report,
            format!(
                "its {} bytes and the {used} bytes the work folder and the session's tmp/ and \
                 home/ already hold pass capabilities.resources.workdir_max_bytes of {} bytes",
                record.bytes, limits.max_bytes
            ),
        );
    }
    let source_path = entry.join(ENTRY_TARGET_DIR);
    let source = match WorkdirRoot::open(&source_path) {
        Ok(source) => source,
        Err(error) => {
            return skipped(
                report,
                format!("cannot open {}: {error}", source_path.display()),
            )
        }
    };

    let seeded = match make_seed_folder(session_home_root) {
        Ok(seeded) => seeded,
        Err(reason) => return skipped(report, reason),
    };
    let destination = match seeded.cache_root.open_beneath(SEED_TARGET_DIR) {
        Ok(Some(destination)) => destination,
        Ok(None) => {
            drop(seeded);
            return skipped(
                report,
                "the seed folder vanished as it was created".to_string(),
            );
        }
        Err(error) => {
            drop(seeded);
            return skipped(report, format!("cannot open the seed folder: {error}"));
        }
    };
    let copied = crate::tree_copy::copy_tree(
        &source,
        &destination,
        &CopyOptions {
            max_bytes: limits.max_bytes.saturating_sub(used),
            excluded: Vec::new(),
            skip_runtime_writes: false,
            unreadable: Unreadable::Fail,
            modes: Modes::Keep,
            reflink: limits.reflink,
        },
    );
    match copied {
        Ok(copied) => {
            report.outcome = SeedOutcome::Seeded;
            report.target_dir = Some(seeded.path.display().to_string());
            report.files = copied.files;
            report.bytes = copied.bytes;
            report.reflinked = copied.reflinked;
            report.copied = copied.copied;
            report.duration_ms = elapsed_ms(started);
            Some(SessionSeed {
                report,
                seeded: Some(seeded),
            })
        }
        Err(error) => {
            drop(seeded);
            // `home/.cache` was created for the seed alone: the home folder is new at staging.
            let _ = session_home_root.remove_dir_all(SEED_CACHE_DIR);
            let reason = match error {
                CopyError::Ceiling { max_bytes, path } => format!(
                    "the copy passes capabilities.resources.workdir_max_bytes of {} bytes at \
                     {path}, with {used} bytes already in the work folder and the session's \
                     tmp/ and home/",
                    max_bytes.saturating_add(used)
                ),
                CopyError::Unreadable { path, source } => format!("cannot read {path}: {source}"),
                CopyError::Io { path, source } => format!("copying into {path} failed: {source}"),
            };
            skipped(report, reason)
        }
    }
}

/// Creates `home/.cache/cargo-target` and holds `home/.cache` for the seed's removal.
fn make_seed_folder(session_home_root: &WorkdirRoot) -> Result<SeededTarget, String> {
    let mode = crate::murmur_home::MURMUR_HOME_DIR_MODE;
    session_home_root
        .create_dir_all(SEED_CACHE_DIR, mode)
        .map_err(|error| format!("cannot create the seed folder: {error}"))?;
    // A root of its own rather than one beneath the home root, so nothing it meets at removal
    // latches a refusal on the session. The home folder was created by this staging and no
    // command has run, so the path names the folder just created.
    let session_cache_root = WorkdirRoot::open(&session_home_root.host_path(SEED_CACHE_DIR))
        .map_err(|error| format!("cannot open the seed folder: {error}"))?;
    let seeded = SeededTarget {
        path: session_cache_root.host_path(SEED_TARGET_DIR),
        cache_root: session_cache_root.clone(),
    };
    session_cache_root
        .create_dir_all(SEED_TARGET_DIR, mode)
        .map_err(|error| format!("cannot create the seed folder: {error}"))?;
    Ok(seeded)
}

/// The bytes of the regular file `Cargo.lock` at `workdir`'s root, opened with no link followed,
/// or `None` when there is none, it is a link or something else, it is larger than
/// [`MAX_LOCKFILE_BYTES`], or it cannot be read. Through the root's own descriptor, so nothing
/// here latches a refusal on the session.
fn read_root_lockfile(workdir: &WorkdirRoot) -> Option<Vec<u8>> {
    let dir = workdir.dir_fd().ok()?;
    let fd = rustix::fs::openat(
        &dir,
        LOCKFILE_NAME,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = std::fs::File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_LOCKFILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_LOCKFILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_LOCKFILE_BYTES).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_copy::tests::{reflinks_in, scratch};
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::time::{Duration, SystemTime};

    const LOCK: &[u8] = b"version = 4\n\n[[package]]\nname = \"dep-a\"\nversion = \"0.1.0\"\n";

    const RUSTC_INFO: &str = r#"{"rustc_fingerprint":1,"outputs":{"1":{"success":true,"status":"","code":0,"stdout":"___\nlib___.rlib\n","stderr":""},"2":{"success":true,"status":"","code":0,"stdout":"rustc 1.95.0 (59807616e 2026-04-14)\nbinary: rustc\ncommit-hash: x\nhost: x86_64-unknown-linux-gnu\nrelease: 1.95.0\n","stderr":""}},"successes":{}}"#;

    struct Fixture {
        _scratch: tempfile::TempDir,
        root: PathBuf,
        murmur_home: PathBuf,
        store: PathBuf,
        lockfile: PathBuf,
        target: PathBuf,
        when: SystemTime,
    }

    impl Fixture {
        /// A host-built target folder beside its `Cargo.lock`, and an empty murmur home.
        fn new() -> Self {
            let scratch = scratch();
            let root = scratch.path().to_path_buf();
            let murmur_home = root.join("home/.murmur");
            let project = root.join("project");
            let target = root.join("warm");
            std::fs::create_dir_all(&project).unwrap();
            let lockfile = project.join(LOCKFILE_NAME);
            std::fs::write(&lockfile, LOCK).unwrap();
            let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            for (rel, bytes, mode) in [
                (
                    "CACHEDIR.TAG",
                    b"Signature: 8a477f597d28d172789f06886806bc55".to_vec(),
                    0o664,
                ),
                (".rustc_info.json", RUSTC_INFO.as_bytes().to_vec(), 0o664),
                ("debug/deps/libdep_a.rlib", vec![9u8; 40_000], 0o664),
                (
                    "debug/build/dep-a/build-script-build",
                    b"#!/bin/sh\n".to_vec(),
                    0o775,
                ),
            ] {
                let path = target.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, bytes).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(when))
                    .unwrap();
            }
            Self {
                _scratch: scratch,
                store: murmur_home.join(BUILD_CACHE_DIR),
                root,
                murmur_home,
                lockfile,
                target,
                when,
            }
        }

        fn add(&self) -> Result<AddedEntry, AddError> {
            add(&self.store, &self.murmur_home, &self.lockfile, &self.target)
        }

        /// A work folder holding `lockfile` at its root, and a session home folder.
        fn session(&self, name: &str, lockfile: Option<&[u8]>) -> (WorkdirRoot, WorkdirRoot) {
            let workdir = self.root.join(name).join("workdir");
            let home = self.root.join(name).join("home");
            std::fs::create_dir_all(&workdir).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            if let Some(bytes) = lockfile {
                std::fs::write(workdir.join(LOCKFILE_NAME), bytes).unwrap();
            }
            (
                WorkdirRoot::open(&workdir).unwrap(),
                WorkdirRoot::open(&home).unwrap(),
            )
        }

        fn seed(
            &self,
            workdir: &WorkdirRoot,
            home: &WorkdirRoot,
            max_bytes: u64,
        ) -> Option<SessionSeed> {
            seed_session(
                workdir,
                home,
                &self.store,
                &SeedLimits {
                    max_bytes,
                    measured: vec![workdir.path(), home.path()],
                    reflink: Reflink::Try,
                },
            )
        }
    }

    fn partials(store: &Path) -> Vec<String> {
        std::fs::read_dir(store)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(PARTIAL_PREFIX))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every file under `dir`, relative, with its bytes.
    fn digest(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.is_file() {
                    files.push((
                        path.strip_prefix(dir).unwrap().display().to_string(),
                        std::fs::read(&path).unwrap(),
                    ));
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn build_cache_lockfile_key_is_the_sha256_of_its_bytes() {
        assert_eq!(
            lockfile_key(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn build_cache_rustc_is_the_first_line_of_the_recorded_rustc_vv() {
        let fixture = Fixture::new();
        assert_eq!(
            rustc_of_target(&fixture.target).as_deref(),
            Some("rustc 1.95.0 (59807616e 2026-04-14)")
        );
        std::fs::remove_file(fixture.target.join(".rustc_info.json")).unwrap();
        assert_eq!(rustc_of_target(&fixture.target), None);
    }

    /// `add` stores the target under the lockfile's SHA-256 with its record, keeps every
    /// modification time, holds every directory and executable at `0700` and every other file at
    /// `0600`, and hard-links nothing to the source.
    #[test]
    fn build_cache_add_stores_a_private_copy_keyed_by_the_lockfile() {
        let fixture = Fixture::new();
        let added = fixture.add().unwrap();
        let key = lockfile_key(LOCK);
        assert_eq!(added.entry, fixture.store.join(&key));
        let record = read_entry_record(&added.entry).unwrap();
        assert_eq!(record, added.record);
        assert_eq!(record.lockfile_sha256, key);
        assert_eq!(
            record.source,
            std::fs::canonicalize(&fixture.target)
                .unwrap()
                .display()
                .to_string()
        );
        assert_eq!(record.files, 4);
        assert_eq!(
            record.bytes,
            digest(&fixture.target)
                .iter()
                .map(|(_, b)| b.len() as u64)
                .sum::<u64>()
        );
        assert_eq!(
            record.rustc.as_deref(),
            Some("rustc 1.95.0 (59807616e 2026-04-14)")
        );
        assert!(chrono::DateTime::parse_from_rfc3339(&record.added_at).is_ok());
        assert!(record.added_at.ends_with('Z'));

        let stored = added.entry.join(ENTRY_TARGET_DIR);
        assert_eq!(digest(&stored), digest(&fixture.target));
        let mode = |path: &Path| std::fs::symlink_metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&fixture.store), 0o700);
        assert_eq!(mode(&added.entry), 0o700);
        assert_eq!(mode(&stored), 0o700);
        assert_eq!(mode(&stored.join("debug/deps")), 0o700);
        assert_eq!(
            mode(&stored.join("debug/build/dep-a/build-script-build")),
            0o700
        );
        assert_eq!(mode(&stored.join("debug/deps/libdep_a.rlib")), 0o600);
        assert_eq!(mode(&added.entry.join(ENTRY_FILE)), 0o600);
        for (rel, _) in digest(&fixture.target) {
            let from = std::fs::metadata(fixture.target.join(&rel)).unwrap();
            let to = std::fs::metadata(stored.join(&rel)).unwrap();
            assert_eq!(to.modified().unwrap(), fixture.when, "{rel}");
            assert_eq!(to.nlink(), 1, "{rel}");
            assert_ne!(from.ino(), to.ino(), "{rel}");
        }
        assert!(partials(&fixture.store).is_empty());

        let listed = list(&fixture.store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, key);
        assert_eq!(listed[0].record.as_ref().unwrap(), &record);
    }

    /// A second `add` of the same lockfile replaces the entry whole, leaving no `.partial-*`.
    #[test]
    fn build_cache_add_again_replaces_the_entry_whole() {
        let fixture = Fixture::new();
        let first = fixture.add().unwrap();
        std::fs::write(first.entry.join("target/stale"), b"from before").unwrap();
        std::fs::write(fixture.target.join("debug/new"), b"new").unwrap();
        let second = fixture.add().unwrap();
        assert_eq!(first.entry, second.entry);
        assert!(!second.entry.join("target/stale").exists());
        assert_eq!(
            std::fs::read(second.entry.join("target/debug/new")).unwrap(),
            b"new"
        );
        assert_eq!(second.record.files, 5);
        assert!(partials(&fixture.store).is_empty());
        assert_eq!(list(&fixture.store).unwrap().len(), 1);
    }

    /// Every refusal names its path and leaves the store as it was: absent here.
    #[test]
    fn build_cache_add_refusals_leave_the_store_unchanged() {
        let fixture = Fixture::new();
        let not_target = fixture.root.join("not-target");
        std::fs::create_dir_all(not_target.join("debug")).unwrap();
        let inside = fixture
            .murmur_home
            .join("records/x/home/.cache/cargo-target");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("CACHEDIR.TAG"), b"x").unwrap();
        let cases: Vec<(PathBuf, PathBuf, &str)> = vec![
            (
                fixture.lockfile.clone(),
                not_target.clone(),
                "neither CACHEDIR.TAG nor .rustc_info.json",
            ),
            (
                fixture.root.join("missing.lock"),
                fixture.target.clone(),
                "does not exist",
            ),
            (
                fixture.root.join("project"),
                fixture.target.clone(),
                "is not a regular file",
            ),
            (
                fixture.lockfile.clone(),
                fixture.root.join("missing"),
                "does not exist",
            ),
            (fixture.lockfile.clone(), inside.clone(), "is inside"),
        ];
        for (lockfile, source, words) in cases {
            let error = add(&fixture.store, &fixture.murmur_home, &lockfile, &source).unwrap_err();
            let message = error.to_string();
            assert!(message.contains(words), "{message}");
            assert!(!fixture.store.exists(), "{message}");
        }
        assert!(list(&fixture.store).unwrap().is_empty());
    }

    /// A work folder whose `Cargo.lock` matches an entry is seeded with a byte-identical copy in
    /// the session home, reported with counts that add up, and the copy is removed when the
    /// holder drops; the store is unchanged by a write to the copy.
    #[test]
    fn build_cache_seed_copies_the_matching_entry_into_the_session_home() {
        let fixture = Fixture::new();
        let added = fixture.add().unwrap();
        let (workdir, home) = fixture.session("a", Some(LOCK));
        let before = digest(&added.entry);
        let seed = fixture.seed(&workdir, &home, u64::MAX).unwrap();
        let report = &seed.report;
        assert_eq!(report.outcome, SeedOutcome::Seeded);
        assert_eq!(report.reason, None);
        assert_eq!(report.lockfile_sha256, lockfile_key(LOCK));
        assert_eq!(report.entry, added.entry.display().to_string());
        let target = home.path().join(SEED_TARGET_REL);
        assert_eq!(
            report.target_dir.as_deref(),
            Some(target.display().to_string().as_str())
        );
        assert_eq!(report.files, 4);
        assert_eq!(report.reflinked + report.copied, report.files);
        if reflinks_in(&fixture.root) {
            assert_eq!(report.reflinked, 4);
        }
        assert_eq!(report.bytes, added.record.bytes);
        assert_eq!(report.rustc, added.record.rustc);
        assert_eq!(digest(&target), digest(&added.entry.join(ENTRY_TARGET_DIR)));
        assert_eq!(seed.seeded.as_ref().unwrap().path(), target);

        std::fs::write(target.join("debug/deps/libdep_a.rlib"), b"junk").unwrap();
        assert_eq!(digest(&added.entry), before);
        assert!(!workdir.path().join("target").exists());

        drop(seed);
        assert!(!target.exists());
        assert!(home.path().exists());
        assert_eq!(digest(&added.entry), before);
    }

    /// Removal at session end unlinks a link a command planted inside the seed, or in place of
    /// it, without following it.
    #[test]
    fn build_cache_seed_removal_never_follows_a_link() {
        let fixture = Fixture::new();
        fixture.add().unwrap();
        let outside = fixture.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("canary"), b"keep").unwrap();

        let (workdir, home) = fixture.session("inside", Some(LOCK));
        let seed = fixture.seed(&workdir, &home, u64::MAX).unwrap();
        let target = home.path().join(SEED_TARGET_REL);
        std::fs::remove_dir_all(target.join("debug")).unwrap();
        symlink(&outside, target.join("debug")).unwrap();
        drop(seed);
        assert!(!target.exists());
        assert_eq!(std::fs::read(outside.join("canary")).unwrap(), b"keep");

        let (workdir, home) = fixture.session("replaced", Some(LOCK));
        let seed = fixture.seed(&workdir, &home, u64::MAX).unwrap();
        let target = home.path().join(SEED_TARGET_REL);
        std::fs::remove_dir_all(&target).unwrap();
        symlink(&outside, &target).unwrap();
        drop(seed);
        assert!(std::fs::symlink_metadata(&target).is_err());
        assert_eq!(std::fs::read(outside.join("canary")).unwrap(), b"keep");
    }

    /// No `Cargo.lock`, one that differs by a byte, one that is a link, or no store: nothing is
    /// looked up, reported or created.
    #[test]
    fn build_cache_seed_is_none_where_nothing_matches() {
        let fixture = Fixture::new();
        fixture.add().unwrap();
        let mut other = LOCK.to_vec();
        other.push(b'\n');
        for (name, lockfile) in [("none", None), ("differs", Some(other.as_slice()))] {
            let (workdir, home) = fixture.session(name, lockfile);
            assert!(fixture.seed(&workdir, &home, u64::MAX).is_none(), "{name}");
            assert!(!home.path().join(".cache").exists(), "{name}");
        }
        let (workdir, home) = fixture.session("link", None);
        symlink(&fixture.lockfile, workdir.path().join(LOCKFILE_NAME)).unwrap();
        assert!(fixture.seed(&workdir, &home, u64::MAX).is_none());
        assert!(workdir.take_refusals().is_empty());

        let (workdir, home) = fixture.session("no-store", Some(LOCK));
        let missing = fixture.root.join("no-such-store");
        assert!(seed_session(
            &workdir,
            &home,
            &missing,
            &SeedLimits {
                max_bytes: u64::MAX,
                measured: vec![],
                reflink: Reflink::Try,
            },
        )
        .is_none());
    }

    /// An entry that would pass the ceiling with what the folders already hold is skipped before
    /// any copy, naming both byte counts and the ceiling; an entry file that cannot be read is
    /// skipped naming it, and leaves no partial seed.
    #[test]
    fn build_cache_seed_skips_an_unusable_entry_and_leaves_nothing() {
        let fixture = Fixture::new();
        let added = fixture.add().unwrap();
        let (workdir, home) = fixture.session("ceiling", Some(LOCK));
        std::fs::write(workdir.path().join("big"), vec![0u8; 1000]).unwrap();
        let used = crate::resources::directory_size_bytes(workdir.path());
        let max = added.record.bytes + used - 1;
        let seed = fixture.seed(&workdir, &home, max).unwrap();
        assert_eq!(seed.report.outcome, SeedOutcome::Skipped);
        assert!(seed.seeded.is_none());
        assert_eq!(seed.report.target_dir, None);
        let reason = seed.report.reason.clone().unwrap();
        assert!(
            reason.contains(&format!("{} bytes", added.record.bytes)),
            "{reason}"
        );
        assert!(reason.contains(&format!("{used} bytes")), "{reason}");
        assert!(
            reason.contains(&format!("workdir_max_bytes of {max} bytes")),
            "{reason}"
        );
        assert!(!home.path().join(".cache").exists());

        if rustix::process::geteuid().is_root() {
            crate::runtime_err!("[SKIP-HOST] running as root: no file is unreadable");
            return;
        }
        let secret = added.entry.join("target/debug/deps/libdep_a.rlib");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        let (workdir, home) = fixture.session("unreadable", Some(LOCK));
        let seed = fixture.seed(&workdir, &home, u64::MAX).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(seed.report.outcome, SeedOutcome::Skipped);
        let reason = seed.report.reason.clone().unwrap();
        assert!(reason.contains(&secret.display().to_string()), "{reason}");
        assert!(!home.path().join(".cache").exists());
        assert!(home.take_refusals().is_empty());
    }

    /// The report's serialized keys are the documented `session_start.build_cache` shape.
    #[test]
    fn build_cache_report_serializes_the_documented_keys() {
        let report = BuildCacheReport {
            outcome: SeedOutcome::Skipped,
            reason: Some("why".to_string()),
            lockfile_sha256: "ab".to_string(),
            entry: "/e".to_string(),
            target_dir: None,
            files: 0,
            bytes: 0,
            reflinked: 0,
            copied: 0,
            duration_ms: 3,
            rustc: None,
        };
        let value = serde_json::to_value(&report).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bytes",
                "copied",
                "duration_ms",
                "entry",
                "files",
                "lockfile_sha256",
                "outcome",
                "reason",
                "reflinked",
                "rustc",
                "target_dir"
            ]
        );
        assert_eq!(value["outcome"], "skipped");
        assert_eq!(value["target_dir"], serde_json::Value::Null);
    }
}
