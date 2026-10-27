//! Facts about the host's `capabilities.shell.allow` binaries, kept between stagings so a warm
//! launch checks them with `stat` instead of working them out again.
//!
//! What [`crate::interpreter_runtime`] works out for an allowlisted binary is read from host files
//! alone: whether the binary is a recognised interpreter, the import path it prints, the entries
//! of each directory on that path, and the `PT_INTERP`, `DT_NEEDED` and `DT_RUNPATH` of every file
//! its extension modules' library closure reaches. The first staging that meets a binary records
//! those facts in one entry under `~/.murmur/host_facts/`, each beside the [`FileStamp`] of the
//! file or directory it was read from. A later staging takes the entry only when every recorded
//! stamp equals a live `stat`, and then starts no interpreter, lists no directory and parses no
//! extension module or library the walk reaches. Anything that differs, or that the entry never
//! recorded, has the binary's facts worked out again from scratch, and that result is what the
//! staging uses and stores.
//!
//! What depends on the staging is never kept. Every launch drops the reported entries under the
//! capsule's writable roots and walks the closure live, skipping paths under those roots, so a
//! kept entry grants exactly what a fresh derivation grants.
//!
//! The directory is read and written on the terms `~/.murmur/compiled` is: a real directory of
//! this uid at [`MURMUR_HOME_DIR_MODE`], each entry a regular file of this uid at
//! [`PRIVATE_FILE_MODE`] opened without following a symlink, and nothing at all when the murmur
//! home or the directory is reachable from one of the capsule's writable roots. An entry that
//! fails any of those checks, or does not parse as this build's format, is worked out again and
//! never trusted. A stamp says which file a fact was read from, not who wrote the entry: code
//! running as this uid outside containment, such as an `advisory` capsule's shell, can write a
//! well-formed entry, as it can a compiled form or `~/.murmur/config.yaml`.
//!
//! CPython and Perl find their prefix from files beside the binary and in the directory above it.
//! The stamps of those two directories change when a file there is added, removed or renamed, not
//! when one is edited in place. [`FileStamp`] states the ctime resolution caveat.
//!
//! [`MURMUR_HOME_DIR_MODE`]: crate::murmur_home::MURMUR_HOME_DIR_MODE
//! [`PRIVATE_FILE_MODE`]: crate::murmur_home::PRIVATE_FILE_MODE

#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use crate::interpreter_runtime::{DerivedInterpreterRuntime, Facts, InterpreterKind};
#[cfg(target_os = "linux")]
use crate::murmur_home::FileStamp;
#[cfg(target_os = "linux")]
use crate::sandbox::ElfDependencies;

/// The format of an entry. Bump it whenever what is derived about a binary, or how, changes: an
/// entry of any other format is worked out again.
#[cfg(target_os = "linux")]
pub(crate) const HOST_FACTS_FORMAT: u32 = 1;

/// The largest entry read. An entry for an interpreter with a thousand extension modules is a few
/// hundred KiB; a larger file is worked out again rather than parsed.
#[cfg(target_os = "linux")]
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;

/// Where stagings keep host facts: `~/.murmur/host_facts`, or a directory a test names.
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct HostFactsCache {
    /// `None` for the murmur home's [`crate::murmur_home::HomeEntryKind::HostFacts`].
    dir: Option<PathBuf>,
}

impl HostFactsCache {
    /// The cache every launch and `mur run --explain-scope` uses.
    pub fn in_murmur_home() -> Self {
        Self { dir: None }
    }

    /// A cache at `dir`, whose parent stands in for the murmur home. For tests, which must never
    /// touch the developer's own home.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    /// The directory this staging reads and writes entries in, created owner-only if missing, or
    /// `None` when the cache is unusable for it: no murmur home, a home or directory reachable
    /// from one of the canonical `roots`, or a directory that is a symlink, another uid's, or
    /// cannot be held at `0700`.
    ///
    /// A directory found wider than owner-only is narrowed and emptied of its files, the way
    /// `~/.murmur/compiled` is, and this staging keeps nothing it held.
    #[cfg(target_os = "linux")]
    pub(crate) fn open(&self, roots: &[PathBuf]) -> Option<Shelf> {
        let reachable = |dir: &Path| {
            roots
                .iter()
                .any(|root| crate::murmur_home::reachable_from(dir, root))
        };
        let (home, name) = match &self.dir {
            None => (
                crate::murmur_home::ensure_murmur_home().ok()?,
                crate::murmur_home::HomeEntryKind::HostFacts
                    .file_name()?
                    .to_string(),
            ),
            Some(dir) => (
                dir.parent()?.to_path_buf(),
                dir.file_name()?.to_str()?.to_string(),
            ),
        };
        let (dir, emptied) = crate::murmur_home::private_subdir(&home, &name, true, reachable)?;
        Some(Shelf { dir, emptied })
    }
}

/// One examined binary, and where this staging's facts about it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFactsUse {
    /// The binary, canonical.
    pub binary: PathBuf,
    pub source: HostFactsSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostFactsSource {
    /// A kept entry, every recorded stamp matching.
    Kept,
    /// Worked out from the binary and its files. `changed_path` is the first recorded path whose
    /// stamp differed, or that the entry never recorded, and is set only for
    /// [`HostFactsReason::Changed`].
    Derived {
        reason: HostFactsReason,
        changed_path: Option<PathBuf>,
    },
}

/// Why a binary's facts were worked out instead of kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFactsReason {
    /// No entry for the binary.
    Absent,
    /// A path the entry recorded has a different stamp now, or this staging reached one it never
    /// recorded.
    Changed,
    /// The entry is not an owner-only regular file of this uid, does not parse, or is of another
    /// format, runtime version, interpreter table, library search list or binary.
    Unreadable,
    /// The cache cannot be used for this staging.
    NotKept,
}

impl HostFactsReason {
    /// The reason as `session_start.host_facts` names it.
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Changed => "changed",
            Self::Unreadable => "unreadable",
            Self::NotKept => "not-kept",
        }
    }
}

/// The cache directory one staging opened.
#[cfg(target_os = "linux")]
pub(crate) struct Shelf {
    dir: PathBuf,
    /// Whether opening it narrowed a wide directory and removed what it held.
    emptied: bool,
}

/// What [`runtime_of`] found for one binary.
#[cfg(target_os = "linux")]
pub(crate) struct RuntimeOf {
    /// `None` when the binary is not a recognised interpreter or its runtime was not derived.
    pub(crate) runtime: Option<DerivedInterpreterRuntime>,
    pub(crate) source: HostFactsSource,
    /// Whether the shelf holds an entry for the binary when the call returns: the kept one, or
    /// the one just stored.
    pub(crate) held: bool,
}

/// `binary`'s runtime, from its kept entry in `shelf` when every recorded stamp matches, and
/// otherwise worked out and stored. With no `shelf` the facts are worked out and nothing is
/// stored. Each line saying an interpreter's runtime was not derived goes to `report`, kept or
/// worked out alike.
#[cfg(target_os = "linux")]
pub(crate) fn runtime_of(
    shelf: Option<&Shelf>,
    binary: &Path,
    kinds: &[InterpreterKind<'_>],
    roots: &[PathBuf],
    timeout: std::time::Duration,
    report: &mut dyn FnMut(&str),
) -> RuntimeOf {
    let derived = |reason, changed_path| HostFactsSource::Derived {
        reason,
        changed_path,
    };
    let Some(shelf) = shelf.filter(|shelf| !shelf.emptied) else {
        let fresh = derive(binary, kinds, roots, timeout, report);
        let held = match (shelf, fresh.entry) {
            (Some(shelf), Some(entry)) => store(&shelf.dir.join(entry_name(binary)), &entry),
            _ => false,
        };
        return RuntimeOf {
            runtime: fresh.runtime,
            source: derived(HostFactsReason::NotKept, None),
            held,
        };
    };
    let path = shelf.dir.join(entry_name(binary));
    let source = match read_entry(&path) {
        Read::Absent => derived(HostFactsReason::Absent, None),
        Read::Unreadable => derived(HostFactsReason::Unreadable, None),
        Read::Entry(entry) => match reuse(&entry, binary, kinds, roots, report) {
            Ok(runtime) => {
                return RuntimeOf {
                    runtime,
                    source: HostFactsSource::Kept,
                    held: true,
                }
            }
            Err(Stale::Unreadable) => derived(HostFactsReason::Unreadable, None),
            Err(Stale::Changed(changed)) => derived(HostFactsReason::Changed, Some(changed)),
        },
    };
    let fresh = derive(binary, kinds, roots, timeout, report);
    let held = fresh.entry.is_some_and(|entry| store(&path, &entry));
    RuntimeOf {
        runtime: fresh.runtime,
        source,
        held,
    }
}

/// `<lowercase hex sha256 of the canonical binary path's bytes>.json`.
#[cfg(target_os = "linux")]
fn entry_name(binary: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    format!(
        "{}.json",
        murmur_artifact::sha256_hex(binary.as_os_str().as_bytes())
    )
}

/// One entry: what was worked out for one binary, and the stamp of everything it was read from.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// [`HOST_FACTS_FORMAT`].
    format: u32,
    /// The deriving capsule-runtime's version.
    runtime_version: String,
    /// The interpreter table the binary was recognised and queried with, verbatim.
    kinds: Vec<KindRecord>,
    /// [`crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS`], verbatim.
    search_dirs: Vec<String>,
    /// The binary, canonical, which the file name was computed for.
    binary: Stamped,
    /// The binary's directory, then the one above it when there is one.
    context: Vec<Stamped>,
    /// The recognised kind's name, or `None` for a binary that is not an interpreter, whose entry
    /// records nothing below.
    kind: Option<String>,
    /// What the completed query printed, in order.
    reported: Vec<String>,
    /// One per [`Self::reported`] string.
    entries: Vec<ReportedEntry>,
    /// Each kept entry of the import path the derivation listed for extension modules.
    dirs: Vec<ListedDir>,
    /// Each file the closure walk opened.
    elf: Vec<ElfFacts>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KindRecord {
    name: String,
    marker_symbol: String,
    args: Vec<String>,
}

#[cfg(target_os = "linux")]
impl KindRecord {
    fn table(kinds: &[InterpreterKind<'_>]) -> Vec<Self> {
        kinds
            .iter()
            .map(|kind| Self {
                name: kind.name.to_string(),
                marker_symbol: kind.marker_symbol.to_string(),
                args: kind.args.iter().map(|arg| arg.to_string()).collect(),
            })
            .collect()
    }
}

/// A path that existed, with its [`FileStamp`] in its `Display` form.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stamped {
    path: PathBuf,
    stamp: String,
}

/// A path with its stamp, `None` when `stat` failed on it.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MaybeStamped {
    path: PathBuf,
    stamp: Option<String>,
}

/// One reported string, and the existing file or directory it resolved to, if any.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportedEntry {
    reported: String,
    canonical: Option<PathBuf>,
    stamp: Option<String>,
}

/// One listed import-path entry: its stamp, the ELF shared objects directly inside it, and every
/// other entry of its listing that is not itself a directory, each stamped through any symlink.
/// Both lists are empty for an entry that could not be listed, such as a zip archive.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListedDir {
    path: PathBuf,
    stamp: Option<String>,
    seeds: Vec<Stamped>,
    others: Vec<MaybeStamped>,
}

/// One file the closure walk opened: its stamp and its dependencies, `None` when it could not be
/// opened or did not parse as ELF64.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ElfFacts {
    path: PathBuf,
    stamp: Option<String>,
    dependencies: Option<ElfDependencies>,
}

/// The live [`FileStamp`] of `path` through any symlink, as every recorded stamp but a seed's
/// handle is taken.
#[cfg(target_os = "linux")]
fn stamp_of(path: &Path) -> Option<String> {
    std::fs::metadata(path)
        .ok()
        .map(|metadata| FileStamp::of(&metadata).to_string())
}

/// The binary's directory and the one above it, stamped. `None` when either cannot be.
#[cfg(target_os = "linux")]
fn context_of(binary: &Path) -> Option<Vec<Stamped>> {
    binary
        .ancestors()
        .skip(1)
        .take(2)
        .map(|dir| {
            Some(Stamped {
                path: dir.to_path_buf(),
                stamp: stamp_of(dir)?,
            })
        })
        .collect()
}

/// `reported` resolved the way [`crate::interpreter_runtime::keep_entries`] resolves it before
/// filtering: absolute, canonical, and an existing directory or regular file.
#[cfg(target_os = "linux")]
fn resolve_reported(reported: &str) -> ReportedEntry {
    let resolved = Some(Path::new(reported))
        .filter(|path| path.is_absolute())
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|canonical| {
            let metadata = std::fs::metadata(&canonical).ok()?;
            (metadata.is_dir() || metadata.is_file())
                .then(|| (canonical, FileStamp::of(&metadata).to_string()))
        });
    let (canonical, stamp) = resolved.unzip();
    ReportedEntry {
        reported: reported.to_string(),
        canonical,
        stamp,
    }
}

/// What one derivation produced: the runtime, and the entry to store when everything it read
/// could be stamped and the query completed.
#[cfg(target_os = "linux")]
struct Fresh {
    runtime: Option<DerivedInterpreterRuntime>,
    entry: Option<Entry>,
}

/// `binary`'s facts worked out from scratch: recognised from its `.dynsym`, queried, filtered
/// against `roots`, and its extension modules' closure walked, every read recorded with its stamp.
/// A query that fails, or leaves nothing to grant, is reported to `report`.
#[cfg(target_os = "linux")]
fn derive(
    binary: &Path,
    kinds: &[InterpreterKind<'_>],
    roots: &[PathBuf],
    timeout: std::time::Duration,
    report: &mut dyn FnMut(&str),
) -> Fresh {
    use crate::interpreter_runtime::{query, recognise, report_not_derived, runtime_from_report};

    // Stamped before anything is read, so a change made while it is read leaves a stamp the next
    // staging does not match.
    let binary_stamp = stamp_of(binary);
    let context = context_of(binary);
    let header = |kind: Option<&str>| {
        Some(Entry {
            format: HOST_FACTS_FORMAT,
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
            kinds: KindRecord::table(kinds),
            search_dirs: search_dirs(),
            binary: Stamped {
                path: binary.to_path_buf(),
                stamp: binary_stamp.clone()?,
            },
            context: context.clone()?,
            kind: kind.map(str::to_string),
            reported: Vec::new(),
            entries: Vec::new(),
            dirs: Vec::new(),
            elf: Vec::new(),
        })
    };
    let Some(kind) = recognise(binary, kinds) else {
        return Fresh {
            runtime: None,
            entry: header(None),
        };
    };
    let reported = match query(binary, kind, timeout) {
        Ok(reported) => reported,
        Err(reason) => {
            report_not_derived(binary, kind, &reason, report);
            return Fresh {
                runtime: None,
                entry: None,
            };
        }
    };
    let entries = reported
        .iter()
        .map(|entry| resolve_reported(entry))
        .collect();
    let mut recorder = Recorder::default();
    let Ok(runtime) = runtime_from_report(binary, kind, &reported, roots, &mut recorder, report);
    // A binary or prefix replaced while the query ran leaves the entry describing neither.
    let unmoved = stamp_of(binary) == binary_stamp && context_of(binary) == context;
    let entry = header(Some(kind.name))
        .filter(|_| unmoved)
        .map(|entry| Entry {
            reported,
            entries,
            dirs: recorder.dirs,
            elf: recorder.elf,
            ..entry
        });
    Fresh { runtime, entry }
}

#[cfg(target_os = "linux")]
fn search_dirs() -> Vec<String> {
    crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
        .iter()
        .map(|dir| dir.to_string())
        .collect()
}

/// The facts a derivation reads, read from the host and recorded with their stamps.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct Recorder {
    dirs: Vec<ListedDir>,
    elf: Vec<ElfFacts>,
}

#[cfg(target_os = "linux")]
impl Facts for Recorder {
    type Changed = std::convert::Infallible;

    fn shared_objects_in(&mut self, dir: &Path) -> Result<Vec<PathBuf>, Self::Changed> {
        use crate::sandbox::{elf_is_shared_object, ElfFile};

        let stamp = stamp_of(dir);
        let mut seeds = Vec::new();
        let mut others = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            // A directory is never a seed, and replacing it changes `dir`'s own stamp.
            if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            let metadata = std::fs::metadata(&path).ok();
            if metadata.as_ref().is_some_and(std::fs::Metadata::is_file) {
                if let Ok(file) = ElfFile::open_host(&path) {
                    if elf_is_shared_object(&file) {
                        seeds.push(Stamped {
                            stamp: FileStamp::of(file.metadata()).to_string(),
                            path,
                        });
                        continue;
                    }
                }
            }
            others.push(MaybeStamped {
                stamp: metadata.map(|metadata| FileStamp::of(&metadata).to_string()),
                path,
            });
        }
        let found = seeds.iter().map(|seed| seed.path.clone()).collect();
        self.dirs.push(ListedDir {
            path: dir.to_path_buf(),
            stamp,
            seeds,
            others,
        });
        Ok(found)
    }

    fn dependencies(&mut self, path: &Path) -> Result<Option<ElfDependencies>, Self::Changed> {
        let (stamp, dependencies) = match crate::sandbox::ElfFile::open_host(path) {
            Ok(file) => (
                Some(FileStamp::of(file.metadata()).to_string()),
                crate::sandbox::parse_elf_dependencies(&file),
            ),
            Err(_) => (stamp_of(path), None),
        };
        self.elf.push(ElfFacts {
            path: path.to_path_buf(),
            stamp,
            dependencies: dependencies.clone(),
        });
        Ok(dependencies)
    }
}

/// The facts a kept entry recorded, each handed out only while its live stamp still matches.
#[cfg(target_os = "linux")]
struct Recorded<'a> {
    dirs: std::collections::HashMap<&'a Path, &'a ListedDir>,
    elf: std::collections::HashMap<&'a Path, &'a ElfFacts>,
}

#[cfg(target_os = "linux")]
impl<'a> Recorded<'a> {
    fn of(entry: &'a Entry) -> Self {
        Self {
            dirs: entry
                .dirs
                .iter()
                .map(|dir| (dir.path.as_path(), dir))
                .collect(),
            elf: entry
                .elf
                .iter()
                .map(|file| (file.path.as_path(), file))
                .collect(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Facts for Recorded<'_> {
    type Changed = PathBuf;

    fn shared_objects_in(&mut self, dir: &Path) -> Result<Vec<PathBuf>, PathBuf> {
        let changed = |path: &Path| path.to_path_buf();
        let listed = self.dirs.get(dir).ok_or_else(|| changed(dir))?;
        if stamp_of(dir) != listed.stamp {
            return Err(changed(dir));
        }
        for seed in &listed.seeds {
            if stamp_of(&seed.path).as_ref() != Some(&seed.stamp) {
                return Err(changed(&seed.path));
            }
        }
        for other in &listed.others {
            if stamp_of(&other.path) != other.stamp {
                return Err(changed(&other.path));
            }
        }
        Ok(listed.seeds.iter().map(|seed| seed.path.clone()).collect())
    }

    fn dependencies(&mut self, path: &Path) -> Result<Option<ElfDependencies>, PathBuf> {
        match self.elf.get(path) {
            Some(file) if stamp_of(path) == file.stamp => Ok(file.dependencies.clone()),
            _ => Err(path.to_path_buf()),
        }
    }
}

#[cfg(target_os = "linux")]
enum Read {
    Absent,
    Unreadable,
    Entry(Box<Entry>),
}

/// The entry at `path`, when it is an owner-only regular file of this uid, reached without a
/// symlink, that parses as this build's entry format.
#[cfg(target_os = "linux")]
fn read_entry(path: &Path) -> Read {
    use std::io::Read as _;

    if std::fs::symlink_metadata(path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
    {
        return Read::Absent;
    }
    let Some((file, metadata)) = crate::murmur_home::open_private(path) else {
        return Read::Unreadable;
    };
    if metadata.len() > MAX_ENTRY_BYTES {
        return Read::Unreadable;
    }
    let mut bytes = Vec::new();
    if file.take(MAX_ENTRY_BYTES).read_to_end(&mut bytes).is_err() {
        return Read::Unreadable;
    }
    match serde_json::from_slice(&bytes) {
        Ok(entry) => Read::Entry(Box::new(entry)),
        Err(_) => Read::Unreadable,
    }
}

/// Why a kept entry was not taken.
#[cfg(target_os = "linux")]
enum Stale {
    Unreadable,
    Changed(PathBuf),
}

/// `binary`'s runtime from `entry`, every recorded stamp checked against a live `stat` and the
/// per-staging part run live against `roots`.
#[cfg(target_os = "linux")]
fn reuse(
    entry: &Entry,
    binary: &Path,
    kinds: &[InterpreterKind<'_>],
    roots: &[PathBuf],
    report: &mut dyn FnMut(&str),
) -> Result<Option<DerivedInterpreterRuntime>, Stale> {
    let foreign = entry.format != HOST_FACTS_FORMAT
        || entry.runtime_version != env!("CARGO_PKG_VERSION")
        || entry.kinds != KindRecord::table(kinds)
        || entry.search_dirs != search_dirs()
        || entry.binary.path != binary
        || entry.entries.len() != entry.reported.len()
        || entry
            .entries
            .iter()
            .zip(&entry.reported)
            .any(|(resolved, reported)| resolved.reported != *reported);
    let context: Vec<&Path> = binary.ancestors().skip(1).take(2).collect();
    let context_paths: Vec<&Path> = entry.context.iter().map(|dir| dir.path.as_path()).collect();
    if foreign || context_paths != context {
        return Err(Stale::Unreadable);
    }
    if stamp_of(binary).as_ref() != Some(&entry.binary.stamp) {
        return Err(Stale::Changed(binary.to_path_buf()));
    }
    for dir in &entry.context {
        if stamp_of(&dir.path).as_ref() != Some(&dir.stamp) {
            return Err(Stale::Changed(dir.path.clone()));
        }
    }
    let Some(name) = &entry.kind else {
        let empty = entry.reported.is_empty() && entry.dirs.is_empty() && entry.elf.is_empty();
        return if empty {
            Ok(None)
        } else {
            Err(Stale::Unreadable)
        };
    };
    let kind = *kinds
        .iter()
        .find(|kind| kind.name == name)
        .ok_or(Stale::Unreadable)?;
    for recorded in &entry.entries {
        let live = resolve_reported(&recorded.reported);
        if live != *recorded {
            let changed = recorded
                .canonical
                .clone()
                .or(live.canonical)
                .unwrap_or_else(|| PathBuf::from(&recorded.reported));
            return Err(Stale::Changed(changed));
        }
    }
    crate::interpreter_runtime::runtime_from_report(
        binary,
        kind,
        &entry.reported,
        roots,
        &mut Recorded::of(entry),
        report,
    )
    .map_err(Stale::Changed)
}

/// Writes `entry` to `path`, owner-only, by rename, and says whether it was written. A failed
/// write leaves the next staging to work the facts out again.
#[cfg(target_os = "linux")]
fn store(path: &Path, entry: &Entry) -> bool {
    serde_json::to_vec(entry)
        .is_ok_and(|bytes| crate::murmur_home::write_private_file(path, &bytes).is_ok())
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::print_stdout)]
mod tests {
    use super::*;
    use crate::interpreter_runtime::tests::{
        cache_in, canonical_tempdir, dash_copy, dash_kind, pad_with_zeros, thread_rchar,
    };
    use crate::interpreter_runtime::{derive_in, InterpreterRuntimes, QUERY_TIMEOUT};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const EIGHT_MIB: u64 = 8 * 1024 * 1024;

    /// A stand-in interpreter at `<root>/prefix/bin/standin` whose query records that it ran in
    /// `<root>/ran` and reports each of `reported`, under a cache at `<root>/host_facts`. The
    /// marker, the cache and every reported directory sit outside the two directories whose
    /// stamps the entry records beside the binary, so writing them never changes the entry's
    /// context.
    struct StandIn {
        _temp: tempfile::TempDir,
        root: PathBuf,
        binary: PathBuf,
        marker: PathBuf,
        script: String,
    }

    impl StandIn {
        fn new(reported: &[&Path]) -> Option<Self> {
            let (temp, root) = canonical_tempdir();
            let binary = dash_copy(&root.join("prefix"), "standin")?;
            let marker = root.join("ran");
            let printed: Vec<String> = reported
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            let script = format!(
                ": > {}; printf '{}'",
                marker.display(),
                printed.join("\\000")
            );
            Some(Self {
                _temp: temp,
                root,
                binary,
                marker,
                script,
            })
        }

        /// `derive_in` for the stand-in alone, with `roots` writable, through `cache`.
        fn derive_with(&self, roots: &[&Path], cache: &HostFactsCache) -> InterpreterRuntimes {
            let args = ["-c", self.script.as_str()];
            let kinds = [dash_kind(&args)];
            derive_in(
                &["standin".to_string()],
                &[],
                roots,
                &[self.binary.parent().unwrap().to_path_buf()],
                &kinds,
                QUERY_TIMEOUT,
                cache,
            )
        }

        fn derive(&self, roots: &[&Path]) -> InterpreterRuntimes {
            self.derive_with(roots, &cache_in(&self.root))
        }

        /// The same derivation through a cache that can never be used.
        fn derive_uncached(&self, roots: &[&Path]) -> InterpreterRuntimes {
            let not_a_dir = self.root.join("not-a-cache");
            std::fs::write(&not_a_dir, b"").unwrap();
            let derived = self.derive_with(roots, &HostFactsCache::at(&not_a_dir));
            assert_eq!(self.source(&derived), not_kept());
            derived
        }

        fn entry(&self) -> PathBuf {
            self.root.join("host_facts").join(entry_name(&self.binary))
        }

        fn source(&self, derived: &InterpreterRuntimes) -> HostFactsSource {
            assert_eq!(derived.host_facts.len(), 1, "{derived:?}");
            assert_eq!(derived.host_facts[0].binary, self.binary);
            derived.host_facts[0].source.clone()
        }

        /// Removes the marker, so whether the next derivation ran the stand-in can be read.
        fn forget_run(&self) {
            let _ = std::fs::remove_file(&self.marker);
        }

        fn ran(&self) -> bool {
            self.marker.exists()
        }
    }

    fn derived(reason: HostFactsReason, changed_path: Option<PathBuf>) -> HostFactsSource {
        HostFactsSource::Derived {
            reason,
            changed_path,
        }
    }

    fn absent() -> HostFactsSource {
        derived(HostFactsReason::Absent, None)
    }

    fn not_kept() -> HostFactsSource {
        derived(HostFactsReason::NotKept, None)
    }

    /// A synthetic ELF64 shared object at `path` needing `needed`, found through `runpath`.
    fn shared_object(path: &Path, needed: &[&str], runpath: Option<&str>) {
        std::fs::write(path, crate::sandbox::build_elf64(None, needed, runpath)).unwrap();
    }

    /// `<root>/lib` holding one extension module that needs only the C library, which the
    /// stand-in's own closure already grants.
    fn lib_with_one_module(root: &Path) -> PathBuf {
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        shared_object(&lib.join("ext.so"), &["libc.so.6"], None);
        lib
    }

    /// Whether `path` is an owner-only regular file holding an entry this build reads.
    fn holds_a_valid_entry(path: &Path) -> bool {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        metadata.is_file()
            && metadata.mode() & 0o777 == 0o600
            && matches!(read_entry(path), Read::Entry(_))
    }

    /// A cold derivation stores an entry; the next staging keeps it, runs nothing and reads a
    /// few KiB, though the binary is padded to 64 MiB.
    #[test]
    fn a_kept_entry_runs_nothing_and_reads_no_binary_in_full() {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        pad_with_zeros(&stand_in.binary, 64 << 20);

        let first = stand_in.derive(&[]);
        assert_eq!(stand_in.source(&first), absent());
        assert!(stand_in.ran());
        assert!(holds_a_valid_entry(&stand_in.entry()));
        assert_eq!(first.runtimes.len(), 1, "{first:?}");
        assert_eq!(first.runtimes[0].dirs, vec![lib.clone()]);

        stand_in.forget_run();
        let before = thread_rchar();
        let second = stand_in.derive(&[]);
        let read = thread_rchar() - before;
        assert_eq!(stand_in.source(&second), HostFactsSource::Kept);
        assert_eq!(second.runtimes, first.runtimes);
        assert!(!stand_in.ran(), "a kept entry ran the interpreter");
        assert!(read < EIGHT_MIB, "the kept staging read {read} bytes");
        println!("a kept staging read {read} bytes");
    }

    /// A binary replaced by rename is worked out again, and its entry rewritten.
    #[test]
    fn a_replaced_binary_is_derived_again() {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        let first = stand_in.derive(&[]);
        assert_eq!(
            stand_in.source(&stand_in.derive(&[])),
            HostFactsSource::Kept
        );
        let stored = std::fs::metadata(stand_in.entry()).unwrap().ino();

        let replacement = stand_in.binary.with_file_name(".standin.new");
        std::fs::copy(&stand_in.binary, &replacement).unwrap();
        std::fs::rename(&replacement, &stand_in.binary).unwrap();
        stand_in.forget_run();
        let again = stand_in.derive(&[]);

        assert_eq!(
            stand_in.source(&again),
            derived(HostFactsReason::Changed, Some(stand_in.binary.clone()))
        );
        assert!(stand_in.ran());
        assert_eq!(again.runtimes, first.runtimes);
        assert_ne!(std::fs::metadata(stand_in.entry()).unwrap().ino(), stored);
        assert!(holds_a_valid_entry(&stand_in.entry()));
        assert_eq!(
            stand_in.source(&stand_in.derive(&[])),
            HostFactsSource::Kept
        );
    }

    /// A module dropped into a kept directory is worked out again, and its closure is granted.
    #[test]
    fn a_module_added_to_a_kept_directory_is_derived_again() {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let deps = root.join("deps");
        std::fs::create_dir(&deps).unwrap();
        std::fs::write(deps.join("libextra.so.1"), b"not elf").unwrap();
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        let first = stand_in.derive(&[]);
        assert_eq!(first.runtimes[0].libraries, Vec::<PathBuf>::new());
        assert_eq!(
            stand_in.source(&stand_in.derive(&[])),
            HostFactsSource::Kept
        );

        shared_object(
            &lib.join("_extra.so"),
            &["libextra.so.1"],
            Some("$ORIGIN/../deps"),
        );
        stand_in.forget_run();
        let again = stand_in.derive(&[]);

        assert_eq!(
            stand_in.source(&again),
            derived(HostFactsReason::Changed, Some(lib.clone()))
        );
        assert!(stand_in.ran());
        assert_eq!(
            again.runtimes[0].libraries,
            vec![deps.join("libextra.so.1")]
        );
        assert_eq!(again.runtimes, stand_in.derive_uncached(&[]).runtimes);
        assert_eq!(
            stand_in.source(&stand_in.derive(&[])),
            HostFactsSource::Kept
        );
    }

    /// Stores a valid entry, damages it with `damage`, and expects the next staging to work
    /// the facts out again, equal to a derivation with no cache, and to leave a valid entry.
    fn an_entry_damaged_by(damage: impl FnOnce(&Path)) {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        let first = stand_in.derive(&[]);
        assert!(holds_a_valid_entry(&stand_in.entry()));

        damage(&stand_in.entry());
        stand_in.forget_run();
        let again = stand_in.derive(&[]);

        assert_eq!(
            stand_in.source(&again),
            derived(HostFactsReason::Unreadable, None)
        );
        assert!(stand_in.ran(), "a damaged entry was trusted");
        assert_eq!(again.runtimes, first.runtimes);
        assert_eq!(again.runtimes, stand_in.derive_uncached(&[]).runtimes);
        assert!(holds_a_valid_entry(&stand_in.entry()));
        assert_eq!(
            stand_in.source(&stand_in.derive(&[])),
            HostFactsSource::Kept
        );
    }

    /// Rewrites the JSON entry at `path` through `edit`, owner-only.
    fn edit_entry(path: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        edit(&mut value);
        crate::murmur_home::write_private_file(path, value.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn an_entry_of_random_bytes_is_derived_again() {
        an_entry_damaged_by(|path| {
            let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
            let bytes: Vec<u8> = (0..4096)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            crate::murmur_home::write_private_file(path, &bytes).unwrap();
        });
    }

    #[test]
    fn a_truncated_entry_is_derived_again() {
        an_entry_damaged_by(|path| {
            let bytes = std::fs::read(path).unwrap();
            crate::murmur_home::write_private_file(path, &bytes[..bytes.len() / 2]).unwrap();
        });
    }

    #[test]
    fn an_entry_of_another_format_is_derived_again() {
        an_entry_damaged_by(|path| {
            edit_entry(path, |entry| {
                entry["format"] = serde_json::json!(HOST_FACTS_FORMAT + 1);
            });
        });
    }

    #[test]
    fn an_entry_from_another_runtime_version_is_derived_again() {
        an_entry_damaged_by(|path| {
            edit_entry(path, |entry| entry["runtime_version"] = "0.0.0".into());
        });
    }

    #[test]
    fn an_entry_from_another_interpreter_table_is_derived_again() {
        an_entry_damaged_by(|path| {
            edit_entry(path, |entry| {
                entry["kinds"][0]["args"] = serde_json::json!(["-c", "exit 0"]);
            });
        });
    }

    #[test]
    fn a_group_readable_entry_is_derived_again() {
        an_entry_damaged_by(|path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        });
    }

    #[test]
    fn an_entry_that_is_a_symlink_to_a_valid_entry_is_derived_again() {
        an_entry_damaged_by(|path| {
            let target = path.with_file_name("elsewhere.json");
            std::fs::rename(path, &target).unwrap();
            std::os::unix::fs::symlink(&target, path).unwrap();
            assert!(holds_a_valid_entry(&target));
        });
    }

    #[test]
    fn an_entry_for_another_binary_is_derived_again() {
        an_entry_damaged_by(|path| {
            edit_entry(path, |entry| {
                entry["binary"]["path"] = "/usr/bin/another".into();
            });
        });
    }

    /// The cache never writes to standard error: a damaged, foreign or unusable entry is worked
    /// out again and prints exactly what a fresh derivation prints.
    #[test]
    fn the_cache_prints_nothing_of_its_own() {
        let source = include_str!("host_facts.rs");
        let production = &source[..source
            .find("#[cfg(all(test, target_os = \"linux\"))]")
            .unwrap()];
        for writer in ["runtime_err!", "eprintln!", "eprint!", "println!", "stderr"] {
            assert!(!production.contains(writer), "host_facts.rs uses {writer}");
        }
    }

    /// The per-staging filtering runs live. An entry derived reporting A and B with `deps`
    /// writable is kept for a staging that can write B, granting A alone, and for one that can
    /// write neither, granting both; a staging whose closure walk reaches the file under `deps`
    /// the entry never read works the facts out again.
    #[test]
    fn a_kept_entry_is_filtered_against_each_stagings_roots() {
        let (_guard, root) = canonical_tempdir();
        let a = root.join("a");
        let b = root.join("b");
        let deps = root.join("deps");
        for dir in [&a, &b, &deps] {
            std::fs::create_dir(dir).unwrap();
        }
        shared_object(
            &a.join("ext.so"),
            &["libextra.so.1"],
            Some("$ORIGIN/../deps"),
        );
        std::fs::write(deps.join("libextra.so.1"), b"not elf").unwrap();
        let Some(stand_in) = StandIn::new(&[&a, &b]) else {
            return;
        };

        let first = stand_in.derive(&[&deps]);
        assert_eq!(stand_in.source(&first), absent());
        assert_eq!(first.runtimes[0].dirs, vec![a.clone(), b.clone()]);
        assert_eq!(first.runtimes[0].libraries, Vec::<PathBuf>::new());

        stand_in.forget_run();
        let without_b = stand_in.derive(&[&deps, &b]);
        assert_eq!(stand_in.source(&without_b), HostFactsSource::Kept);
        assert_eq!(without_b.runtimes[0].dirs, vec![a.clone()]);
        assert_eq!(
            without_b.runtimes,
            stand_in.derive_uncached(&[&deps, &b]).runtimes
        );
        stand_in.forget_run();
        let both = stand_in.derive(&[&deps]);
        assert_eq!(stand_in.source(&both), HostFactsSource::Kept);
        assert_eq!(both.runtimes, first.runtimes);
        assert!(!stand_in.ran());

        let reached = stand_in.derive(&[]);
        assert_eq!(
            stand_in.source(&reached),
            derived(HostFactsReason::Changed, Some(deps.join("libextra.so.1")))
        );
        assert!(stand_in.ran());
        assert_eq!(
            reached.runtimes[0].libraries,
            vec![deps.join("libextra.so.1")]
        );
        assert_eq!(reached.runtimes, stand_in.derive_uncached(&[]).runtimes);
    }

    /// A cache under one of the capsule's writable roots is neither read nor written.
    #[test]
    fn a_cache_under_a_writable_root_is_never_read_or_written() {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        let first = stand_in.derive(&[]);
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        crate::murmur_home::hold_private_dir(&work.join("host_facts")).unwrap();
        let planted = work.join("host_facts").join(entry_name(&stand_in.binary));
        std::fs::copy(stand_in.entry(), &planted).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o600)).unwrap();
        let planted_stamp = stamp_of(&planted);

        stand_in.forget_run();
        let under = stand_in.derive_with(&[&work], &HostFactsCache::at(work.join("host_facts")));
        assert_eq!(stand_in.source(&under), not_kept());
        assert!(stand_in.ran(), "the entry under the writable root was read");
        assert_eq!(under.runtimes, first.runtimes);
        assert_eq!(stamp_of(&planted), planted_stamp, "the entry was rewritten");
        assert_eq!(
            std::fs::read_dir(work.join("host_facts")).unwrap().count(),
            1
        );

        // A cache that does not exist yet under a writable root is not created there either.
        let fresh = work.join("fresh");
        let uncreated =
            stand_in.derive_with(&[&work], &HostFactsCache::at(fresh.join("host_facts")));
        assert_eq!(stand_in.source(&uncreated), not_kept());
        assert!(!fresh.join("host_facts").exists());
    }

    /// A cache directory reached through a symlink is never used, and one wider than
    /// owner-only is narrowed and emptied before anything is kept in it.
    #[test]
    fn a_symlinked_or_wide_cache_directory_keeps_nothing() {
        let (_guard, root) = canonical_tempdir();
        let lib = lib_with_one_module(&root);
        let Some(stand_in) = StandIn::new(&[&lib]) else {
            return;
        };
        let first = stand_in.derive(&[]);
        let entry_file = entry_name(&stand_in.binary);

        let real = root.join("real");
        crate::murmur_home::hold_private_dir(&real).unwrap();
        std::fs::copy(stand_in.entry(), real.join(&entry_file)).unwrap();
        std::fs::set_permissions(
            real.join(&entry_file),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let real_stamp = stamp_of(&real.join(&entry_file));
        let link_home = root.join("linked");
        std::fs::create_dir(&link_home).unwrap();
        std::os::unix::fs::symlink(&real, link_home.join("host_facts")).unwrap();
        stand_in.forget_run();
        let linked = stand_in.derive_with(&[], &HostFactsCache::at(link_home.join("host_facts")));
        assert_eq!(stand_in.source(&linked), not_kept());
        assert!(stand_in.ran());
        assert_eq!(linked.runtimes, first.runtimes);
        assert_eq!(stamp_of(&real.join(&entry_file)), real_stamp);

        let wide_home = root.join("wide");
        let wide = wide_home.join("host_facts");
        std::fs::create_dir_all(&wide).unwrap();
        std::fs::set_permissions(&wide, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::copy(stand_in.entry(), wide.join(&entry_file)).unwrap();
        std::fs::set_permissions(
            wide.join(&entry_file),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        // A second link outside the cache keeps the planted inode allocated, so the entry written
        // after the narrowing cannot be handed its number, as ext4 does with a just-freed inode.
        let planted_link = root.join("planted-entry");
        std::fs::hard_link(wide.join(&entry_file), &planted_link).unwrap();
        let planted_ino = std::fs::metadata(&planted_link).unwrap().ino();
        std::fs::write(wide.join("planted.json"), b"{}").unwrap();
        std::fs::create_dir(wide.join("subdir")).unwrap();
        stand_in.forget_run();
        let narrowed = stand_in.derive_with(&[], &HostFactsCache::at(&wide));
        assert_eq!(stand_in.source(&narrowed), not_kept());
        assert!(stand_in.ran(), "an entry in a wide directory was trusted");
        assert_eq!(narrowed.runtimes, first.runtimes);
        assert_eq!(
            std::fs::symlink_metadata(&wide).unwrap().mode() & 0o777,
            0o700
        );
        assert!(!wide.join("planted.json").exists());
        assert!(wide.join("subdir").is_dir());
        assert_ne!(
            std::fs::metadata(wide.join(&entry_file)).unwrap().ino(),
            planted_ino,
            "the entry found in the wide directory was kept"
        );
        assert_eq!(std::fs::metadata(&planted_link).unwrap().nlink(), 1);
        assert!(holds_a_valid_entry(&wide.join(&entry_file)));
        let kept = stand_in.derive_with(&[], &HostFactsCache::at(&wide));
        assert_eq!(stand_in.source(&kept), HostFactsSource::Kept);
    }
}
