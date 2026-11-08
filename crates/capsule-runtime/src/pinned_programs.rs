//! The program store, `~/.murmur/programs/`: the pinned downloads a tool's `programs:` block names
//! ([`murmur_artifact::parse_program_downloads`]), fetched once by `mur install`, and which copy of
//! each pinned program a session runs.
//!
//! ```text
//! ~/.murmur/programs/
//!   <sha256>/              content-addressed by the declared sha256, shared by every artifact
//!     record.json          PinnedRecord
//!     files/               the unpacked zip, or the single downloaded file
//!     bin/<program>        relative symlink to ../files/<run>, one per program name
//!   .partial-<random>/     an install in flight; never read, removed on any failure
//! ```
//!
//! Every directory is `0700`; every file `0700` when it is executable and `0600` otherwise. An
//! entry is assembled in a `.partial-*` folder and moved into place by one `rename`, so a reader
//! sees a whole entry or none.
//!
//! The store is written by `mur install` alone ([`commit_download`]). Staging only reads it
//! ([`resolve_pinned_programs`]): a host program of the same name on `PATH` wins; otherwise a
//! copy whose `record.json` pins the declared sha256 and whose `bin/<program>` resolves to an
//! executable file is used, without re-hashing the tree, so launch time does not grow with the
//! download. `mur install` and `mur doctor` verify the whole tree ([`verify_entry`]).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use murmur_artifact::{LockOrigin, PinnedDownload, ProgramDownloads};
use serde::{Deserialize, Serialize};

/// The store's name under `~/.murmur`.
pub const PROGRAMS_DIR: &str = "programs";

/// An entry's record of what was fetched.
pub const RECORD_FILE: &str = "record.json";

/// The folder of an entry holding the downloaded program.
pub const FILES_DIR: &str = "files";

/// The folder of an entry holding one link per program name, appended to a session's `PATH`.
pub const BIN_DIR: &str = "bin";

/// The name prefix of an install in flight.
pub const PARTIAL_PREFIX: &str = ".partial-";

/// The most bytes a download, and the tree unpacked from it, may hold: 2 GiB.
pub const MAX_PINNED_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The mode of an executable file in the store; every other file is
/// [`crate::murmur_home::PRIVATE_FILE_MODE`] and every directory
/// [`crate::murmur_home::MURMUR_HOME_DIR_MODE`].
pub const EXECUTABLE_FILE_MODE: u32 = 0o700;

/// The file a download streams into inside its `.partial-*` folder.
const DOWNLOAD_FILE: &str = "download";

/// `~/.murmur/programs`, whether or not it exists. Creates nothing.
pub fn program_store_root() -> Result<PathBuf, String> {
    Ok(crate::state_store::murmur_home_dir()?.join(PROGRAMS_DIR))
}

/// `~/.murmur/programs`, created owner-only (and `~/.murmur` with it) when missing.
pub fn ensure_program_store() -> Result<PathBuf, String> {
    let root = crate::murmur_home::ensure_murmur_home()?.join(PROGRAMS_DIR);
    crate::murmur_home::hold_private_dir(&root)
        .map_err(|reason| format!("{}: {reason}", root.display()))?;
    Ok(root)
}

/// `record.json`: what an entry was fetched from, and the digest of what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedRecord {
    pub url: String,
    /// The SHA-256 of the downloaded bytes, the entry's folder name.
    pub sha256: String,
    pub platform: String,
    /// The file the program runs, relative to `files/`, for a zip; `None` for a single file.
    pub run: Option<String>,
    /// The SHA-256 of [`Self::files`] rendered as sorted `<path>\t<x|->\t<sha256>\n` lines.
    pub tree_sha256: String,
    /// When the entry was committed, RFC 3339 UTC.
    pub fetched_at: String,
    /// Every regular file under `files/`, sorted by path.
    pub files: Vec<TreeFile>,
}

impl PinnedRecord {
    /// The program file, relative to `files/`: `run`, or the single downloaded file.
    pub fn program_file(&self) -> Option<&str> {
        match &self.run {
            Some(run) => Some(run.as_str()),
            None => self.files.first().map(|file| file.path.as_str()),
        }
    }
}

/// One regular file under an entry's `files/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TreeFile {
    /// Relative to `files/`, `/`-separated.
    pub path: String,
    /// Whether the owner execute bit is set.
    pub executable: bool,
    pub sha256: String,
}

/// The SHA-256 over `files` as sorted `<path>\t<x|->\t<sha256>\n` lines.
pub fn tree_digest(files: &[TreeFile]) -> String {
    let mut sorted: Vec<&TreeFile> = files.iter().collect();
    sorted.sort();
    let lines: String = sorted
        .into_iter()
        .map(|file| {
            let mark = if file.executable { "x" } else { "-" };
            format!("{}\t{mark}\t{}\n", file.path, file.sha256)
        })
        .collect();
    murmur_artifact::sha256_hex(lines.as_bytes())
}

/// Why committing a download failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinnedStoreError {
    /// The download is not a tree the store takes: a missing `run`, a symlink or escaping entry, a
    /// tree over [`MAX_PINNED_DOWNLOAD_BYTES`], or bytes that are no zip.
    #[error("{0}")]
    Unpack(String),
    /// The store could not be written.
    #[error("{0}")]
    Store(String),
}

/// An install in flight: `~/.murmur/programs/.partial-<random>/`, removed when dropped
/// uncommitted.
#[derive(Debug)]
pub struct PartialDownload {
    dir: PathBuf,
    kept: bool,
}

impl PartialDownload {
    /// Creates a fresh owner-only `.partial-*` folder under `store_root`.
    pub fn begin(store_root: &Path) -> Result<Self, PinnedStoreError> {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random)
            .map_err(|err| PinnedStoreError::Store(format!("no randomness for a name: {err}")))?;
        let dir = store_root.join(format!(
            "{PARTIAL_PREFIX}{:016x}",
            u64::from_be_bytes(random)
        ));
        create_private_dir(&dir)?;
        Ok(Self { dir, kept: false })
    }

    /// The file the download streams into.
    pub fn download_path(&self) -> PathBuf {
        self.dir.join(DOWNLOAD_FILE)
    }

    /// Opens [`Self::download_path`] for writing, owner-only.
    pub fn create_download_file(&self) -> Result<std::fs::File, PinnedStoreError> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(crate::murmur_home::PRIVATE_FILE_MODE)
            .open(self.download_path())
            .map_err(|err| store_error(&self.download_path(), err))
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// What [`commit_download`] left in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedEntry {
    /// `<store_root>/<sha256>`.
    pub entry: PathBuf,
    /// The program file, canonical.
    pub program_path: PathBuf,
    /// `true` when a concurrent install stored the entry first and ours was discarded.
    pub raced: bool,
}

/// Unpacks a download whose bytes already hash to `download.sha256` and moves it into
/// `<store_root>/<sha256>/` with its record and `bin/<program>` link.
///
/// `partial` holds the downloaded bytes at [`PartialDownload::download_path`]. With `run`, they
/// are a zip: every entry is unpacked under `files/` with group and other bits stripped; a
/// symlink entry, an absolute or `..` entry name, a tree over [`MAX_PINNED_DOWNLOAD_BYTES`] or a
/// `run` naming no regular file refuses ([`PinnedStoreError::Unpack`]). Without `run`, the bytes
/// are the program, stored executable under the URL's last path segment. The record is written
/// last and the folder renamed into place; when a concurrent install got there first, ours is
/// discarded and theirs verified. Nothing is left behind on any failure.
pub fn commit_download(
    store_root: &Path,
    download: &PinnedDownload,
    program: &str,
    partial: PartialDownload,
) -> Result<CommittedEntry, PinnedStoreError> {
    commit_download_capped(
        store_root,
        download,
        program,
        partial,
        MAX_PINNED_DOWNLOAD_BYTES,
    )
}

pub(crate) fn commit_download_capped(
    store_root: &Path,
    download: &PinnedDownload,
    program: &str,
    mut partial: PartialDownload,
    cap: u64,
) -> Result<CommittedEntry, PinnedStoreError> {
    let entry = entry_path(store_root, &download.sha256)?;
    let files = partial.dir.join(FILES_DIR);
    create_private_dir(&files)?;
    let downloaded = partial.download_path();
    let program_file = match &download.run {
        Some(run) => {
            unpack_zip(&downloaded, &files, cap)?;
            let target = files.join(run);
            match std::fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_file() => {}
                Ok(_) => {
                    return Err(PinnedStoreError::Unpack(format!(
                        "run names {run}, which is not a regular file in the zip"
                    )))
                }
                Err(_) => {
                    return Err(PinnedStoreError::Unpack(format!(
                        "run names {run}, and the zip holds no such file"
                    )))
                }
            }
            std::fs::remove_file(&downloaded).map_err(|err| store_error(&downloaded, err))?;
            run.clone()
        }
        None => {
            let size = std::fs::metadata(&downloaded)
                .map_err(|err| store_error(&downloaded, err))?
                .len();
            if size > cap {
                return Err(PinnedStoreError::Unpack(format!(
                    "the download is {size} bytes, over the {cap}-byte cap"
                )));
            }
            let name = single_file_name(&download.url);
            let target = files.join(&name);
            std::fs::rename(&downloaded, &target).map_err(|err| store_error(&target, err))?;
            set_mode(&target, EXECUTABLE_FILE_MODE)?;
            name
        }
    };
    let tree = list_tree(&files).map_err(PinnedStoreError::Store)?;
    link_program(&partial.dir, program, &program_file)?;
    let record = PinnedRecord {
        url: download.url.clone(),
        sha256: download.sha256.clone(),
        platform: download.platform.clone(),
        run: download.run.clone(),
        tree_sha256: tree_digest(&tree),
        fetched_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        files: tree,
    };
    write_record(&partial.dir, &record)?;

    match std::fs::rename(&partial.dir, &entry) {
        Ok(()) => {
            partial.kept = true;
            let program_path = std::fs::canonicalize(entry.join(FILES_DIR).join(&program_file))
                .map_err(|err| store_error(&entry, err))?;
            Ok(CommittedEntry {
                entry,
                program_path,
                raced: false,
            })
        }
        Err(err) if entry.is_dir() => {
            drop(partial);
            match verify_entry(store_root, download) {
                EntryState::Present { path, .. } => {
                    ensure_bin_link(store_root, download, program)?;
                    Ok(CommittedEntry {
                        entry,
                        program_path: path,
                        raced: true,
                    })
                }
                EntryState::Missing => Err(store_error(&entry, err)),
                EntryState::ChecksumFailed { reason } => Err(PinnedStoreError::Store(format!(
                    "{} was stored by another install and fails its checksum: {reason}",
                    entry.display()
                ))),
            }
        }
        Err(err) => Err(store_error(&entry, err)),
    }
}

/// What the store holds for one download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    /// The tree verifies against its record and the record pins the declared sha256.
    Present {
        /// The program file, canonical.
        path: PathBuf,
        record: PinnedRecord,
    },
    /// No entry for the declared sha256.
    Missing,
    /// An entry that fails verification, and why: a file changed, missing or added since install,
    /// or an unreadable record.
    ChecksumFailed { reason: String },
}

/// Verifies the entry for `download.sha256` in full: reads its record, re-hashes every file under
/// `files/` and compares. Reads only.
pub fn verify_entry(store_root: &Path, download: &PinnedDownload) -> EntryState {
    let Ok(entry) = entry_path(store_root, &download.sha256) else {
        return EntryState::Missing;
    };
    if std::fs::symlink_metadata(&entry).is_err() {
        return EntryState::Missing;
    }
    let failed = |reason: String| EntryState::ChecksumFailed { reason };
    let record = match read_record(&entry) {
        Ok(record) => record,
        Err(reason) => return failed(reason),
    };
    if record.sha256 != download.sha256 {
        return failed(format!(
            "{RECORD_FILE} names sha256 {}, not the pinned {}",
            record.sha256, download.sha256
        ));
    }
    let tree = match list_tree(&entry.join(FILES_DIR)) {
        Ok(tree) => tree,
        Err(reason) => return failed(reason),
    };
    if let Some(reason) = tree_difference(&record.files, &tree) {
        return failed(reason);
    }
    if tree_digest(&tree) != record.tree_sha256 {
        return failed(format!(
            "{RECORD_FILE}'s tree_sha256 does not match the files it lists"
        ));
    }
    let Some(program_file) = record.program_file() else {
        return failed(format!("{RECORD_FILE} names no program file"));
    };
    match std::fs::canonicalize(entry.join(FILES_DIR).join(program_file)) {
        Ok(path) => EntryState::Present { path, record },
        Err(_) => failed(format!(
            "{FILES_DIR}/{program_file} is missing since install"
        )),
    }
}

/// Adds `bin/<program>` to the verified entry for `download` when it is missing or points
/// elsewhere. `Ok(true)` when a link was written.
pub fn ensure_bin_link(
    store_root: &Path,
    download: &PinnedDownload,
    program: &str,
) -> Result<bool, PinnedStoreError> {
    let entry = entry_path(store_root, &download.sha256)?;
    let record = read_record(&entry).map_err(PinnedStoreError::Store)?;
    let program_file = record
        .program_file()
        .ok_or_else(|| PinnedStoreError::Store(format!("{RECORD_FILE} names no program file")))?
        .to_string();
    let link = entry.join(BIN_DIR).join(program);
    if std::fs::read_link(&link).ok() == Some(link_target(&program_file)) {
        return Ok(false);
    }
    if std::fs::symlink_metadata(&link).is_ok() {
        std::fs::remove_file(&link).map_err(|err| store_error(&link, err))?;
    }
    link_program(&entry, program, &program_file)?;
    Ok(true)
}

/// Removes the entry for `sha256`, whatever state it is in.
pub fn remove_entry(store_root: &Path, sha256: &str) -> Result<(), PinnedStoreError> {
    let entry = entry_path(store_root, sha256)?;
    match std::fs::remove_dir_all(&entry) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(store_error(&entry, err)),
    }
}

/// Reads an entry's `record.json`.
pub fn read_record(entry: &Path) -> Result<PinnedRecord, String> {
    let path = entry.join(RECORD_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("{RECORD_FILE} cannot be read: {err}"))?;
    serde_json::from_str(&text).map_err(|err| format!("{RECORD_FILE} cannot be parsed: {err}"))
}

/// One staged tool's pinned downloads, with the lock origin of its pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredToolDownloads {
    /// The artifact as the capsule's manifest names it, scope included.
    pub artifact: String,
    pub origin: LockOrigin,
    /// Only the programs that pin a download.
    pub programs: Vec<ProgramDownloads>,
}

/// Which copy of a pinned program a session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinnedSource {
    /// The store's copy: `bin_dir` joins the end of every command's `PATH`, `tree` is granted
    /// listable and executable, never writable.
    Pinned {
        /// The program file, canonical.
        path: PathBuf,
        /// `<tree>/bin`.
        bin_dir: PathBuf,
        /// The entry, `~/.murmur/programs/<sha256>`, canonical.
        tree: PathBuf,
        url: String,
        sha256: String,
    },
    /// A host program of that name on `PATH`, canonical; it runs as it would with no download.
    Host { path: PathBuf },
    /// Nothing runs under that name from the store, and why.
    Missing { reason: String },
}

impl PinnedSource {
    /// `"pinned"`, `"host"` or `"missing"`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pinned { .. } => "pinned",
            Self::Host { .. } => "host",
            Self::Missing { .. } => "missing",
        }
    }
}

/// One gated program's source choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPinnedProgram {
    pub artifact: String,
    pub program: String,
    pub platform: String,
    pub source: PinnedSource,
}

impl ResolvedPinnedProgram {
    /// `(bin_dir, tree)` for a [`PinnedSource::Pinned`] choice.
    pub fn pinned_dirs(&self) -> Option<(&Path, &Path)> {
        match &self.source {
            PinnedSource::Pinned { bin_dir, tree, .. } => Some((bin_dir, tree)),
            _ => None,
        }
    }
}

/// The `bin/` folder of every [`PinnedSource::Pinned`] choice, distinct, in order.
pub fn pinned_bin_dirs(programs: &[ResolvedPinnedProgram]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (bin_dir, _) in programs
        .iter()
        .filter_map(ResolvedPinnedProgram::pinned_dirs)
    {
        if !dirs.iter().any(|dir| dir == bin_dir) {
            dirs.push(bin_dir.to_path_buf());
        }
    }
    dirs
}

/// The entry of every [`PinnedSource::Pinned`] choice, distinct, in order.
pub fn pinned_trees(programs: &[ResolvedPinnedProgram]) -> Vec<PathBuf> {
    let mut trees: Vec<PathBuf> = Vec::new();
    for (_, tree) in programs
        .iter()
        .filter_map(ResolvedPinnedProgram::pinned_dirs)
    {
        if !trees.iter().any(|known| known == tree) {
            trees.push(tree.to_path_buf());
        }
    }
    trees
}

/// The source of each program `tools` pin a download of for this host, for the programs
/// `shell_allow` grants, against mur's own `PATH` and `~/.murmur/programs`.
///
/// Reads only, never fetches, never fails: a program the store holds no copy of is
/// [`PinnedSource::Missing`].
pub fn derive_pinned_programs(
    tools: &[DeclaredToolDownloads],
    shell_allow: &[String],
) -> Vec<ResolvedPinnedProgram> {
    let store_root = program_store_root();
    resolve_pinned_programs(
        tools,
        shell_allow,
        &host_path_dirs(),
        store_root.as_deref().map_err(String::as_str),
        murmur_artifact::host_download_platform(),
    )
}

/// mur's own `PATH`, split.
pub fn host_path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

/// [`derive_pinned_programs`] with the host injected.
///
/// Only a tool whose `origin` is [`LockOrigin::Operator`] contributes, only for a program key
/// equal to an entry of `shell_allow`, and only when it pins a download for `platform`. One
/// element per (artifact, program), tools in the order given and programs in declaration order.
/// Per program: [`PinnedSource::Host`] when `path_dirs` hold an executable of that name; else
/// [`PinnedSource::Pinned`] when the entry's record pins the declared sha256 and `bin/<program>`
/// resolves to an executable file inside it; else [`PinnedSource::Missing`].
pub fn resolve_pinned_programs(
    tools: &[DeclaredToolDownloads],
    shell_allow: &[String],
    path_dirs: &[PathBuf],
    store_root: Result<&Path, &str>,
    platform: Option<&str>,
) -> Vec<ResolvedPinnedProgram> {
    let Some(platform) = platform else {
        return Vec::new();
    };
    let mut resolved = Vec::new();
    for (tool, program) in gated_programs(tools, shell_allow) {
        let Some(download) = program.for_platform(platform) else {
            continue;
        };
        let source = match host_program(&program.program, path_dirs) {
            Some(path) => PinnedSource::Host { path },
            None => stored_source(store_root, download, &program.program),
        };
        resolved.push(ResolvedPinnedProgram {
            artifact: tool.artifact.clone(),
            program: program.program.clone(),
            platform: platform.to_string(),
            source,
        });
    }
    resolved
}

/// The programs of `tools` a session may run a pinned copy of, whatever the platform: only from a
/// tool whose `origin` is [`LockOrigin::Operator`], and only for a program key equal to an entry
/// of `shell_allow`. Tools in the order given, programs in declaration order.
pub fn gated_programs<'a>(
    tools: &'a [DeclaredToolDownloads],
    shell_allow: &'a [String],
) -> impl Iterator<Item = (&'a DeclaredToolDownloads, &'a ProgramDownloads)> + 'a {
    tools
        .iter()
        .filter(|tool| tool.origin == LockOrigin::Operator)
        .flat_map(move |tool| {
            tool.programs
                .iter()
                .filter(move |program| shell_allow.contains(&program.program))
                .map(move |program| (tool, program))
        })
}

/// The first executable named `program` in `path_dirs`, canonical: the one `execvp` would run.
pub fn host_program(program: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    path_dirs
        .iter()
        .map(|dir| dir.join(program))
        .find(|candidate| crate::sandbox::is_executable_file(candidate))
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
}

/// The store's copy for `download`, checked as far as staging checks it: the record and the
/// link, never the tree's digest.
fn stored_source(
    store_root: Result<&Path, &str>,
    download: &PinnedDownload,
    program: &str,
) -> PinnedSource {
    let missing = |reason: String| PinnedSource::Missing { reason };
    let sha256 = &download.sha256;
    let store_root = match store_root {
        Ok(root) => root,
        Err(reason) => {
            return missing(format!("the program store cannot be located: {reason}"));
        }
    };
    let Ok(entry) = entry_path(store_root, sha256) else {
        return missing(format!("sha256 {sha256} cannot name a store entry"));
    };
    let Ok(tree) = std::fs::canonicalize(&entry) else {
        return missing(format!(
            "the store holds no copy for sha256 {sha256}; run mur install"
        ));
    };
    let record = match read_record(&tree) {
        Ok(record) => record,
        Err(reason) => {
            return missing(format!(
                "the store's copy for sha256 {sha256} has no usable record ({reason}); run mur \
                 install"
            ))
        }
    };
    if record.sha256 != *sha256 {
        return missing(format!(
            "the store's record for sha256 {sha256} names sha256 {}; run mur install",
            record.sha256
        ));
    }
    let link = tree.join(BIN_DIR).join(program);
    let runnable = std::fs::canonicalize(&link)
        .ok()
        .filter(|path| path.starts_with(tree.join(FILES_DIR)))
        .filter(|path| crate::sandbox::is_executable_file(path));
    match runnable {
        Some(path) => PinnedSource::Pinned {
            path,
            bin_dir: tree.join(BIN_DIR),
            tree,
            url: record.url,
            sha256: sha256.clone(),
        },
        None => missing(format!(
            "the store's copy for sha256 {sha256} has no runnable {BIN_DIR}/{program}; run mur \
             install"
        )),
    }
}

/// `<store_root>/<sha256>`, refusing a `sha256` that is not 64 lowercase hex digits.
fn entry_path(store_root: &Path, sha256: &str) -> Result<PathBuf, PinnedStoreError> {
    if !murmur_artifact::is_sha256_hex(sha256) {
        return Err(PinnedStoreError::Store(format!(
            "'{sha256}' is not a sha256 the store names an entry by"
        )));
    }
    Ok(store_root.join(sha256))
}

/// The file name a single-file download is stored under: the URL's last path segment, or
/// `download` when it has none usable.
fn single_file_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_string))
        })
        .filter(|name| {
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
        })
        .unwrap_or_else(|| DOWNLOAD_FILE.to_string())
}

fn link_target(program_file: &str) -> PathBuf {
    Path::new("..").join(FILES_DIR).join(program_file)
}

/// Writes `<entry>/bin/<program>` -> `../files/<program_file>`.
fn link_program(entry: &Path, program: &str, program_file: &str) -> Result<(), PinnedStoreError> {
    let bin = entry.join(BIN_DIR);
    if !bin.is_dir() {
        create_private_dir(&bin)?;
    }
    let link = bin.join(program);
    std::os::unix::fs::symlink(link_target(program_file), &link)
        .map_err(|err| store_error(&link, err))
}

fn write_record(entry: &Path, record: &PinnedRecord) -> Result<(), PinnedStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = entry.join(RECORD_FILE);
    let text = serde_json::to_string_pretty(record)
        .map_err(|err| PinnedStoreError::Store(format!("{RECORD_FILE}: {err}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(crate::murmur_home::PRIVATE_FILE_MODE)
        .open(&path)
        .map_err(|err| store_error(&path, err))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| store_error(&path, err))
}

/// Unpacks the zip at `zip_path` under `into`, refusing what the store does not take.
fn unpack_zip(zip_path: &Path, into: &Path, cap: u64) -> Result<(), PinnedStoreError> {
    let unpack = PinnedStoreError::Unpack;
    let file = std::fs::File::open(zip_path).map_err(|err| store_error(zip_path, err))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|err| unpack(format!("the download is no zip: {err}")))?;
    let mut total: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| unpack(format!("zip entry {index} cannot be read: {err}")))?;
        let name = entry
            .name()
            .map_err(|err| unpack(format!("zip entry {index} has no readable name: {err}")))?
            .to_string();
        if entry.is_symlink() {
            return Err(unpack(format!(
                "zip entry '{name}' is a symlink; a pinned download unpacks only files and folders"
            )));
        }
        if name.starts_with('/') || name.contains('\\') {
            return Err(unpack(format!(
                "zip entry '{name}' escapes the archive's folder"
            )));
        }
        let relative = murmur_artifact::zip_guard::normalized_entry_name(&name)
            .map_err(|_| unpack(format!("zip entry '{name}' escapes the archive's folder")))?;
        if relative.is_empty() {
            continue;
        }
        let target = into.join(&relative);
        if entry.is_dir() {
            create_private_dirs(into, &target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            create_private_dirs(into, parent)?;
        }
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        let mode = if executable {
            EXECUTABLE_FILE_MODE
        } else {
            crate::murmur_home::PRIVATE_FILE_MODE
        };
        let mut out = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&target)
                .map_err(|err| match err.kind() {
                    std::io::ErrorKind::AlreadyExists => {
                        unpack(format!("zip entry '{name}' appears more than once"))
                    }
                    _ => store_error(&target, err),
                })?
        };
        let remaining = cap.saturating_sub(total);
        let written = std::io::copy(&mut (&mut entry).take(remaining + 1), &mut out)
            .map_err(|err| unpack(format!("zip entry '{name}' cannot be unpacked: {err}")))?;
        total += written;
        if total > cap {
            return Err(unpack(format!(
                "the unpacked tree is over the {cap}-byte cap"
            )));
        }
        set_mode(&target, mode)?;
    }
    Ok(())
}

/// Creates `dir` and every missing folder between `root` and it, each owner-only.
fn create_private_dirs(root: &Path, dir: &Path) -> Result<(), PinnedStoreError> {
    let Ok(relative) = dir.strip_prefix(root) else {
        return Err(PinnedStoreError::Unpack(format!(
            "{} escapes the archive's folder",
            dir.display()
        )));
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(PinnedStoreError::Unpack(format!(
                    "zip entry folder {} is also a file",
                    current.display()
                )))
            }
            Err(_) => create_private_dir(&current)?,
        }
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<(), PinnedStoreError> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(crate::murmur_home::MURMUR_HOME_DIR_MODE)
        .create(dir)
        .map_err(|err| store_error(dir, err))?;
    set_mode(dir, crate::murmur_home::MURMUR_HOME_DIR_MODE)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), PinnedStoreError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|err| store_error(path, err))
}

/// Every regular file under `files`, sorted by path; refuses anything but files and folders.
fn list_tree(files: &Path) -> Result<Vec<TreeFile>, String> {
    let mut found = Vec::new();
    walk(files, files, &mut found)?;
    found.sort();
    Ok(found)
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<TreeFile>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let entries = std::fs::read_dir(dir)
        .map_err(|err| format!("{} cannot be read: {err}", shown(root, dir)))?;
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|err| err.to_string())?;
        if metadata.is_dir() {
            walk(root, &path, found)?;
        } else if metadata.is_file() {
            found.push(TreeFile {
                path: relative_name(root, &path),
                executable: metadata.permissions().mode() & 0o100 != 0,
                sha256: file_sha256(&path)
                    .map_err(|err| format!("{} cannot be read: {err}", shown(root, &path)))?,
            });
        } else {
            return Err(format!(
                "{} is not a regular file or folder",
                shown(root, &path)
            ));
        }
    }
    Ok(())
}

/// The first difference between the files a record lists and the files on disk.
fn tree_difference(recorded: &[TreeFile], actual: &[TreeFile]) -> Option<String> {
    for file in recorded {
        match actual.iter().find(|other| other.path == file.path) {
            None => {
                return Some(format!(
                    "{FILES_DIR}/{} is missing since install",
                    file.path
                ))
            }
            Some(other) if other != file => {
                return Some(format!("{FILES_DIR}/{} changed since install", file.path))
            }
            Some(_) => {}
        }
    }
    actual
        .iter()
        .find(|file| !recorded.iter().any(|other| other.path == file.path))
        .map(|file| format!("{FILES_DIR}/{} was added since install", file.path))
}

fn relative_name(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn shown(root: &Path, path: &Path) -> String {
    let relative = relative_name(root, path);
    if relative.is_empty() {
        FILES_DIR.to_string()
    } else {
        format!("{FILES_DIR}/{relative}")
    }
}

/// The SHA-256 of the file at `path`, lowercase hex, read in chunks.
fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    murmur_artifact::sha256_hex_of_reader(&mut file).map(|(_, digest)| digest)
}

fn store_error(path: &Path, err: std::io::Error) -> PinnedStoreError {
    PinnedStoreError::Store(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const PROGRAM: &str = "fixture-prog";

    /// A zip of `entries`: (name, unix mode, contents); a mode with `0o120000` is a symlink.
    fn zip_of(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, mode, contents) in entries {
            if mode & 0o170000 == 0o120000 {
                writer
                    .add_symlink(
                        *name,
                        std::str::from_utf8(contents).unwrap(),
                        zip::write::SimpleFileOptions::default(),
                    )
                    .unwrap();
                continue;
            }
            let options = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn fixture_zip() -> Vec<u8> {
        zip_of(&[
            (
                "fixture/fixture-prog",
                0o755,
                b"#!/bin/sh\necho pinned-copy-ran\n",
            ),
            ("fixture/data.txt", 0o644, b"data\n"),
        ])
    }

    fn sha_of(bytes: &[u8]) -> String {
        murmur_artifact::sha256_hex(bytes)
    }

    fn download_for(bytes: &[u8], run: Option<&str>) -> PinnedDownload {
        PinnedDownload {
            platform: "linux-x86_64".to_string(),
            url: "http://127.0.0.1:1/fixture-prog.zip".to_string(),
            sha256: sha_of(bytes),
            run: run.map(str::to_string),
        }
    }

    fn partial_with(store: &Path, bytes: &[u8]) -> PartialDownload {
        let partial = PartialDownload::begin(store).unwrap();
        partial
            .create_download_file()
            .unwrap()
            .write_all(bytes)
            .unwrap();
        partial
    }

    fn commit(
        store: &Path,
        bytes: &[u8],
        run: Option<&str>,
    ) -> Result<CommittedEntry, PinnedStoreError> {
        commit_download(
            store,
            &download_for(bytes, run),
            PROGRAM,
            partial_with(store, bytes),
        )
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    fn no_partials(store: &Path) -> bool {
        std::fs::read_dir(store).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(PARTIAL_PREFIX)
        })
    }

    #[test]
    fn pinned_programs_commit_unpacks_links_records_and_strips_modes() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let bytes = fixture_zip();
        let committed = commit(store, &bytes, Some("fixture/fixture-prog")).unwrap();
        let entry = store.join(sha_of(&bytes));
        assert_eq!(committed.entry, entry);
        assert!(!committed.raced);
        assert_eq!(
            committed.program_path,
            std::fs::canonicalize(entry.join("files/fixture/fixture-prog")).unwrap()
        );
        assert_eq!(mode_of(&entry), 0o700);
        assert_eq!(mode_of(&entry.join("files")), 0o700);
        assert_eq!(mode_of(&entry.join("files/fixture")), 0o700);
        assert_eq!(mode_of(&entry.join("files/fixture/fixture-prog")), 0o700);
        assert_eq!(mode_of(&entry.join("files/fixture/data.txt")), 0o600);
        assert_eq!(mode_of(&entry.join("record.json")), 0o600);
        assert!(!entry.join(DOWNLOAD_FILE).exists());
        assert_eq!(
            std::fs::read_link(entry.join("bin/fixture-prog")).unwrap(),
            PathBuf::from("../files/fixture/fixture-prog")
        );
        let record = read_record(&entry).unwrap();
        assert_eq!(record.sha256, sha_of(&bytes));
        assert_eq!(record.run.as_deref(), Some("fixture/fixture-prog"));
        assert_eq!(record.platform, "linux-x86_64");
        assert_eq!(record.files.len(), 2);
        assert_eq!(record.tree_sha256, tree_digest(&record.files));
        let lines = format!(
            "fixture/data.txt\t-\t{}\nfixture/fixture-prog\tx\t{}\n",
            sha_of(b"data\n"),
            sha_of(b"#!/bin/sh\necho pinned-copy-ran\n")
        );
        assert_eq!(record.tree_sha256, sha_of(lines.as_bytes()));
        assert!(no_partials(store));
        assert!(matches!(
            verify_entry(store, &download_for(&bytes, Some("fixture/fixture-prog"))),
            EntryState::Present { .. }
        ));
    }

    #[test]
    fn pinned_programs_a_single_file_is_stored_executable_under_the_url_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let bytes = b"#!/bin/sh\necho single\n";
        let committed = commit(store, bytes, None).unwrap();
        let entry = store.join(sha_of(bytes));
        assert_eq!(
            committed.program_path,
            std::fs::canonicalize(entry.join("files/fixture-prog.zip")).unwrap()
        );
        assert_eq!(mode_of(&entry.join("files/fixture-prog.zip")), 0o700);
        assert_eq!(
            std::fs::read_link(entry.join("bin/fixture-prog")).unwrap(),
            PathBuf::from("../files/fixture-prog.zip")
        );
        assert_eq!(single_file_name("https://h.example/a/b/tool?x=1"), "tool");
        assert_eq!(single_file_name("https://h.example/"), "download");
    }

    #[test]
    fn pinned_programs_commit_refuses_a_symlink_an_escape_a_missing_run_and_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let cases: Vec<(Vec<u8>, &str, &str)> = vec![
            (
                zip_of(&[
                    ("fixture/fixture-prog", 0o755, b"x"),
                    ("fixture/link", 0o120777, b"/etc/passwd"),
                ]),
                "fixture/fixture-prog",
                "zip entry 'fixture/link' is a symlink",
            ),
            (
                zip_of(&[
                    ("../escape", 0o644, b"x"),
                    ("fixture/fixture-prog", 0o755, b"x"),
                ]),
                "fixture/fixture-prog",
                "zip entry '../escape' escapes the archive's folder",
            ),
            (
                zip_of(&[("fixture/fixture-prog", 0o755, b"x")]),
                "fixture/absent",
                "run names fixture/absent, and the zip holds no such file",
            ),
            (
                zip_of(&[("fixture/fixture-prog/", 0o755, b"")]),
                "fixture/fixture-prog",
                "run names fixture/fixture-prog, which is not a regular file in the zip",
            ),
            (
                zip_of(&[
                    ("fixture/fixture-prog", 0o755, b"x"),
                    ("./fixture/fixture-prog", 0o755, b"y"),
                ]),
                "fixture/fixture-prog",
                "zip entry './fixture/fixture-prog' appears more than once",
            ),
            (
                b"not a zip".to_vec(),
                "fixture/fixture-prog",
                "the download is no zip",
            ),
        ];
        for (bytes, run, expected) in cases {
            let err = commit(store, &bytes, Some(run)).unwrap_err();
            assert!(matches!(err, PinnedStoreError::Unpack(_)), "{err:?}");
            assert!(err.to_string().contains(expected), "{err}");
            assert!(!store.join(sha_of(&bytes)).exists());
            assert!(no_partials(store));
        }
        let bytes = zip_of(&[("fixture/fixture-prog", 0o755, &[7u8; 64])]);
        let err = commit_download_capped(
            store,
            &download_for(&bytes, Some("fixture/fixture-prog")),
            PROGRAM,
            partial_with(store, &bytes),
            32,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PinnedStoreError::Unpack("the unpacked tree is over the 32-byte cap".to_string())
        );
        assert!(no_partials(store));
    }

    #[test]
    fn pinned_programs_verify_names_a_changed_missing_or_added_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let bytes = fixture_zip();
        let download = download_for(&bytes, Some("fixture/fixture-prog"));
        assert_eq!(verify_entry(store, &download), EntryState::Missing);
        commit(store, &bytes, Some("fixture/fixture-prog")).unwrap();
        let entry = store.join(&download.sha256);

        std::fs::write(entry.join("files/fixture/data.txt"), "changed\n").unwrap();
        assert_eq!(
            verify_entry(store, &download),
            EntryState::ChecksumFailed {
                reason: "files/fixture/data.txt changed since install".to_string()
            }
        );
        std::fs::write(entry.join("files/fixture/data.txt"), "data\n").unwrap();
        assert!(matches!(
            verify_entry(store, &download),
            EntryState::Present { .. }
        ));

        std::fs::set_permissions(
            entry.join("files/fixture/data.txt"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert_eq!(
            verify_entry(store, &download),
            EntryState::ChecksumFailed {
                reason: "files/fixture/data.txt changed since install".to_string()
            }
        );
        std::fs::remove_file(entry.join("files/fixture/data.txt")).unwrap();
        assert_eq!(
            verify_entry(store, &download),
            EntryState::ChecksumFailed {
                reason: "files/fixture/data.txt is missing since install".to_string()
            }
        );
        std::fs::write(entry.join("files/fixture/data.txt"), "data\n").unwrap();
        std::fs::set_permissions(
            entry.join("files/fixture/data.txt"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(entry.join("files/extra"), "x").unwrap();
        assert_eq!(
            verify_entry(store, &download),
            EntryState::ChecksumFailed {
                reason: "files/extra was added since install".to_string()
            }
        );
        std::fs::remove_file(entry.join("files/extra")).unwrap();
        std::fs::write(entry.join("record.json"), "{").unwrap();
        assert!(matches!(
            verify_entry(store, &download),
            EntryState::ChecksumFailed { reason } if reason.starts_with("record.json cannot be parsed")
        ));
    }

    #[test]
    fn pinned_programs_a_concurrent_commit_keeps_theirs_and_links_ours() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path();
        let bytes = fixture_zip();
        let download = download_for(&bytes, Some("fixture/fixture-prog"));
        let ours = partial_with(store, &bytes);
        commit(store, &bytes, Some("fixture/fixture-prog")).unwrap();
        let committed = commit_download(store, &download, "other-name", ours).unwrap();
        assert!(committed.raced);
        let entry = store.join(&download.sha256);
        assert_eq!(
            std::fs::read_link(entry.join("bin/other-name")).unwrap(),
            PathBuf::from("../files/fixture/fixture-prog")
        );
        assert!(no_partials(store));
        assert!(!ensure_bin_link(store, &download, "other-name").unwrap());
        std::fs::remove_file(entry.join("bin/fixture-prog")).unwrap();
        assert!(ensure_bin_link(store, &download, PROGRAM).unwrap());
        remove_entry(store, &download.sha256).unwrap();
        assert!(!entry.exists());
        remove_entry(store, &download.sha256).unwrap();
        assert!(remove_entry(store, "../x").is_err());
    }

    fn tool(origin: LockOrigin, download: &PinnedDownload, program: &str) -> DeclaredToolDownloads {
        DeclaredToolDownloads {
            artifact: "local/pinned-tool".to_string(),
            origin,
            programs: vec![ProgramDownloads {
                program: program.to_string(),
                downloads: vec![download.clone()],
            }],
        }
    }

    fn allow(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn pinned_programs_resolve_follows_the_gate_host_first_then_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("programs");
        std::fs::create_dir(&store).unwrap();
        let bytes = fixture_zip();
        let download = download_for(&bytes, Some("fixture/fixture-prog"));
        let empty_path: Vec<PathBuf> = vec![dir.path().join("no-such-dir")];
        let resolve = |tools: &[DeclaredToolDownloads], allowed: &[&str], path: &[PathBuf]| {
            resolve_pinned_programs(
                tools,
                &allow(allowed),
                path,
                Ok(&store),
                Some("linux-x86_64"),
            )
        };
        let operator = [tool(LockOrigin::Operator, &download, PROGRAM)];

        // Nothing stored: missing, naming the sha and `mur install`.
        let missing = resolve(&operator, &["bash", PROGRAM], &empty_path);
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].source,
            PinnedSource::Missing {
                reason: format!(
                    "the store holds no copy for sha256 {}; run mur install",
                    download.sha256
                )
            }
        );

        commit(&store, &bytes, Some("fixture/fixture-prog")).unwrap();
        let tree = std::fs::canonicalize(store.join(&download.sha256)).unwrap();
        let pinned = resolve(&operator, &[PROGRAM], &empty_path);
        assert_eq!(
            pinned[0].source,
            PinnedSource::Pinned {
                path: tree.join("files/fixture/fixture-prog"),
                bin_dir: tree.join("bin"),
                tree: tree.clone(),
                url: download.url.clone(),
                sha256: download.sha256.clone(),
            }
        );
        assert_eq!(pinned_bin_dirs(&pinned), vec![tree.join("bin")]);
        assert_eq!(pinned_trees(&pinned), vec![tree.clone()]);

        // The gate: not in shell.allow, spelled differently, a runtime pin, another platform.
        assert!(resolve(&operator, &["bash"], &empty_path).is_empty());
        assert!(resolve(&operator, &["Fixture-prog"], &empty_path).is_empty());
        let runtime = [tool(
            LockOrigin::Runtime {
                session: "ses_x".to_string(),
            },
            &download,
            PROGRAM,
        )];
        assert!(resolve(&runtime, &[PROGRAM], &empty_path).is_empty());
        assert!(resolve_pinned_programs(
            &operator,
            &allow(&[PROGRAM]),
            &empty_path,
            Ok(&store),
            Some("darwin-aarch64"),
        )
        .is_empty());
        assert!(resolve_pinned_programs(
            &operator,
            &allow(&[PROGRAM]),
            &empty_path,
            Ok(&store),
            None
        )
        .is_empty());

        // A host program of that name wins.
        let host_dir = dir.path().join("host-bin");
        std::fs::create_dir(&host_dir).unwrap();
        let host = host_dir.join(PROGRAM);
        std::fs::write(&host, "#!/bin/sh\necho host-copy-ran\n").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();
        let chosen = resolve(&operator, &[PROGRAM], std::slice::from_ref(&host_dir));
        assert_eq!(
            chosen[0].source,
            PinnedSource::Host {
                path: std::fs::canonicalize(&host).unwrap()
            }
        );
        assert!(pinned_bin_dirs(&chosen).is_empty());

        // A record naming another sha, and a missing link, are missing.
        let record_path = tree.join(RECORD_FILE);
        let record_text = std::fs::read_to_string(&record_path).unwrap();
        std::fs::write(
            &record_path,
            record_text.replace(&download.sha256, &"ab".repeat(32)),
        )
        .unwrap();
        assert!(matches!(
            &resolve(&operator, &[PROGRAM], &empty_path)[0].source,
            PinnedSource::Missing { reason } if reason.contains(&format!("names sha256 {}", "ab".repeat(32)))
        ));
        std::fs::write(&record_path, record_text).unwrap();
        std::fs::remove_file(tree.join("bin").join(PROGRAM)).unwrap();
        assert!(matches!(
            &resolve(&operator, &[PROGRAM], &empty_path)[0].source,
            PinnedSource::Missing { reason } if reason.contains("has no runnable bin/fixture-prog")
        ));
        let store_err = resolve_pinned_programs(
            &operator,
            &allow(&[PROGRAM]),
            &empty_path,
            Err("HOME is not set"),
            Some("linux-x86_64"),
        );
        assert!(matches!(
            &store_err[0].source,
            PinnedSource::Missing { reason } if reason.contains("HOME is not set")
        ));
    }
}
