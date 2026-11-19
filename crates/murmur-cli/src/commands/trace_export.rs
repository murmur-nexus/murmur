//! `mur trace export`, and `mur trace verify --bundle`.
//!
//! A bundle (`murmur.bundle/1`) is one ended session's sealed record laid out as a records
//! directory — streams with their seals, `blobs/`, `attachments/`, and the conversation record the
//! trace seal names under `conversations/<record>/<context-id>/` — plus `bundle.json`, which lists
//! every other file with its size and SHA-256. Because the root is a records directory,
//! `record_seal::verify_session_records` and `trace_attach::attachment_entries` check it
//! unchanged.
//!
//! Only the seals are signed. `bundle.json` is not, so it lets a recipient notice a file that
//! changed or went missing in transit, never one withheld by whoever exported the bundle.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
};

use capsule_runtime::record_seal::{
    self, single_segment, ConversationPointer, RecordReport, Verdict, BLOBS_DIR, COMPLETION_STREAM,
    CONVERSATIONS_DIR, CONVERSATION_RECORD_FILE, EVAL_STREAM, SEAL_SUFFIX, TRACE_STREAM,
};
use capsule_runtime::retention::now_ms;
use capsule_runtime::running;
use capsule_runtime::{EntryKind, WorkdirError, WorkdirRoot};
use murmur_artifact::is_sha256_hex;
use murmur_artifact::zip_guard::{
    max_artifact_decompressed_bytes, normalized_entry_name, read_zip_entry_capped,
};
use serde::{Deserialize, Serialize};

use crate::commands::trace::{
    conversations_root, print_record_report, record_public_key, records_dir_of, resolve_session,
    running_record, stream_finding, verify_records, worst_stream, E_TRC_001, E_TRC_003, E_TRC_004,
    E_TRC_005, E_TRC_008, E_TRC_009,
};
use crate::commands::trace_attach::{self, ATTACHMENTS_DIR, ATTACHMENT_FORMAT};
use crate::error::{CliError, E_RUN_024};
use crate::live_address::records_unreadable;

/// The `format` of every `bundle.json` this build writes and the only one `--bundle` reads.
pub(crate) const BUNDLE_FORMAT: &str = "murmur.bundle/1";

/// The manifest at the bundle root. The one file of a bundle its own `files` does not list.
pub(crate) const BUNDLE_MANIFEST: &str = "bundle.json";

/// Mode of every directory an export or a zip extraction creates.
const BUNDLE_DIR_MODE: u32 = 0o700;

/// Mode of every file an export or a zip extraction writes: blobs are the unredacted wire payload.
const BUNDLE_FILE_MODE: u32 = 0o600;

/// `S_IFMT`, `S_IFREG` and `S_IFLNK` of a zip entry's unix mode.
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;

// ── bundle.json ───────────────────────────────────────────────────────────────

/// `bundle.json`. Field order is the serialized key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleManifest {
    /// [`BUNDLE_FORMAT`].
    pub(crate) format: String,
    /// `trace.jsonl` line 1's `session_id`, else the trace seal's.
    pub(crate) session_id: Option<String>,
    /// Milliseconds since the epoch.
    pub(crate) exported_at: u64,
    /// `mur <version>`.
    pub(crate) exported_by: String,
    pub(crate) formats: BundleFormats,
    /// `trace.jsonl.seal`'s `key_id`; `None` with no readable trace seal.
    pub(crate) key_id: Option<String>,
    /// The trace seal's pointer, when the conversation record it names was packed.
    pub(crate) conversation: Option<ConversationPointer>,
    /// Sorted by `path`.
    pub(crate) files: Vec<BundleFile>,
}

/// The formats of the files inside a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleFormats {
    /// [`record_seal::SEAL_FORMAT`].
    pub(crate) seal: String,
    /// [`ATTACHMENT_FORMAT`].
    pub(crate) attachment: String,
}

/// One packed file: `path` relative to the bundle root with `/` separators, `sha256` bare
/// lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleFile {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

impl BundleManifest {
    /// A manifest for `files`, sorted by path.
    pub(crate) fn new(
        session_id: Option<String>,
        key_id: Option<String>,
        conversation: Option<ConversationPointer>,
        mut files: Vec<BundleFile>,
    ) -> Self {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            format: BUNDLE_FORMAT.to_string(),
            session_id,
            exported_at: now_ms(),
            exported_by: format!("mur {}", env!("CARGO_PKG_VERSION")),
            formats: BundleFormats {
                seal: record_seal::SEAL_FORMAT.to_string(),
                attachment: ATTACHMENT_FORMAT.to_string(),
            },
            key_id,
            conversation,
            files,
        }
    }

    /// Pretty-printed JSON and a trailing newline.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        text.into_bytes()
    }
}

// ── mur trace export ──────────────────────────────────────────────────────────

/// Where a packed file is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Records,
    Conversation,
}

/// One file to pack: read from `rel` in its source root, written at `path` in the bundle.
#[derive(Debug, Clone)]
struct PlannedFile {
    source: Source,
    rel: String,
    path: String,
}

/// The conversation context directory an export reads from: opened beneath the conversations
/// root, its path for messages, and the trace seal's pointer that named it.
struct ConversationSource {
    root: WorkdirRoot,
    dir: PathBuf,
    pointer: ConversationPointer,
}

/// What an export packs, decided before anything is written.
struct ExportPlan {
    records: WorkdirRoot,
    records_dir: PathBuf,
    conversation: Option<ConversationSource>,
    files: Vec<PlannedFile>,
    key_id: Option<String>,
}

/// `mur trace export --json`.
#[derive(Debug, Serialize)]
struct ExportOutput<'a> {
    session_id: Option<&'a str>,
    path: String,
    form: &'static str,
    files: usize,
    bytes: u64,
    verdict: Verdict,
}

fn refused(rel: &str, dir: &Path, what: &str) -> CliError {
    CliError::new(
        E_TRC_008,
        format!("{rel} in {} {what}; nothing was exported", dir.display()),
    )
}

fn unreadable(rel: &str, dir: &Path, error: &WorkdirError) -> CliError {
    match error {
        WorkdirError::Refused(_) => refused(rel, dir, "is reached through a symbolic link"),
        WorkdirError::Io { .. } => refused(rel, dir, &format!("cannot be read: {error}")),
    }
}

/// Whether `rel` exists as a regular file in `root`; a link or any other file type refuses the
/// export.
fn regular_file(root: &WorkdirRoot, dir: &Path, rel: &str) -> Result<bool, CliError> {
    match root.entry_kind(rel) {
        Ok(None) => Ok(false),
        Ok(Some(EntryKind::File)) => Ok(true),
        Ok(Some(EntryKind::Symlink)) => Err(refused(rel, dir, "is a symbolic link")),
        Ok(Some(_)) => Err(refused(rel, dir, "is not a regular file")),
        Err(error) => Err(unreadable(rel, dir, &error)),
    }
}

/// The file names of the directory `rel` in `root` to pack: every regular file whose name
/// `wanted` accepts. Subdirectories are skipped; a link or other file type among the accepted
/// names refuses the export, and so does a link at `rel` itself.
fn files_in(
    root: &WorkdirRoot,
    dir: &Path,
    rel: &str,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<String>, CliError> {
    match root.entry_kind(rel) {
        Ok(None) => return Ok(Vec::new()),
        Ok(Some(EntryKind::Directory)) => {}
        Ok(Some(EntryKind::Symlink)) => return Err(refused(rel, dir, "is a symbolic link")),
        Ok(Some(_)) => return Err(refused(rel, dir, "is not a directory")),
        Err(error) => return Err(unreadable(rel, dir, &error)),
    }
    let entries = match root.list_dir(rel) {
        Ok(entries) => entries.unwrap_or_default(),
        Err(error) => return Err(unreadable(rel, dir, &error)),
    };
    let mut names = Vec::new();
    for (name, kind) in entries {
        if kind == EntryKind::Directory || !wanted(&name) {
            continue;
        }
        let entry = format!("{rel}/{name}");
        match kind {
            EntryKind::File => names.push(name),
            EntryKind::Symlink => return Err(refused(&entry, dir, "is a symbolic link")),
            _ => return Err(refused(&entry, dir, "is not a regular file")),
        }
    }
    Ok(names)
}

/// Every stream with its seal, the blobs and the attachments of `records`.
fn plan_records(records: &WorkdirRoot, dir: &Path) -> Result<Vec<PlannedFile>, CliError> {
    let mut files = Vec::new();
    for stream in [TRACE_STREAM, EVAL_STREAM, COMPLETION_STREAM] {
        for rel in [stream.to_string(), format!("{stream}{SEAL_SUFFIX}")] {
            if regular_file(records, dir, &rel)? {
                files.push(PlannedFile {
                    source: Source::Records,
                    path: rel.clone(),
                    rel,
                });
            }
        }
    }
    for name in files_in(records, dir, BLOBS_DIR, |_| true)? {
        let rel = format!("{BLOBS_DIR}/{name}");
        files.push(PlannedFile {
            source: Source::Records,
            path: rel.clone(),
            rel,
        });
    }
    for name in files_in(records, dir, ATTACHMENTS_DIR, |name| {
        name.ends_with(".json")
    })? {
        let rel = format!("{ATTACHMENTS_DIR}/{name}");
        files.push(PlannedFile {
            source: Source::Records,
            path: rel.clone(),
            rel,
        });
    }
    Ok(files)
}

/// The conversation record `pointer` names under this host's conversations root, with its seal
/// and blobs, or `None` when the record is not here.
fn plan_conversation(
    pointer: &ConversationPointer,
) -> Result<Option<(ConversationSource, Vec<PlannedFile>)>, CliError> {
    if !single_segment(&pointer.record) || !single_segment(&pointer.context_id) {
        return Ok(None);
    }
    let Some(root_path) = conversations_root() else {
        return Ok(None);
    };
    let Ok(conversations) = WorkdirRoot::open(&root_path) else {
        return Ok(None);
    };
    let rel = format!("{}/{}", pointer.record, pointer.context_id);
    let dir = root_path.join(&rel);
    let context = match conversations.open_beneath(&rel) {
        Ok(Some(context)) => context,
        Ok(None) => return Ok(None),
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(unreadable(&rel, &root_path, &error)),
    };
    if !regular_file(&context, &dir, CONVERSATION_RECORD_FILE)? {
        return Ok(None);
    }
    let prefix = format!("{CONVERSATIONS_DIR}/{rel}");
    let mut files = Vec::new();
    let seal = format!("{CONVERSATION_RECORD_FILE}{SEAL_SUFFIX}");
    let mut rels = vec![CONVERSATION_RECORD_FILE.to_string()];
    if regular_file(&context, &dir, &seal)? {
        rels.push(seal);
    }
    for name in files_in(&context, &dir, BLOBS_DIR, |_| true)? {
        rels.push(format!("{BLOBS_DIR}/{name}"));
    }
    for rel in rels {
        files.push(PlannedFile {
            source: Source::Conversation,
            path: format!("{prefix}/{rel}"),
            rel,
        });
    }
    Ok(Some((
        ConversationSource {
            root: context,
            dir,
            pointer: pointer.clone(),
        },
        files,
    )))
}

fn plan_export(records_dir: &Path) -> Result<ExportPlan, CliError> {
    let records = WorkdirRoot::open(records_dir).map_err(|error| {
        CliError::new(
            E_TRC_008,
            format!(
                "{} cannot be opened: {error}; nothing was exported",
                records_dir.display()
            ),
        )
    })?;
    if !regular_file(&records, records_dir, TRACE_STREAM)? {
        return Err(CliError::new(
            E_TRC_001,
            format!(
                "{} holds no trace.jsonl to export; nothing was exported",
                records_dir.display()
            ),
        ));
    }
    let mut files = plan_records(&records, records_dir)?;
    let seal = record_seal::read_seal(&records, TRACE_STREAM)
        .ok()
        .flatten();
    let mut conversation = None;
    if let Some(pointer) = seal.as_ref().and_then(|seal| seal.conversation.as_ref()) {
        if let Some((source, packed)) = plan_conversation(pointer)? {
            files.extend(packed);
            conversation = Some(source);
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ExportPlan {
        records,
        records_dir: records_dir.to_path_buf(),
        conversation,
        files,
        key_id: seal.map(|seal| seal.key_id),
    })
}

impl ExportPlan {
    /// The bytes of `file`, read through its root.
    fn read(&self, file: &PlannedFile) -> Result<Vec<u8>, CliError> {
        let (root, dir) = match (file.source, &self.conversation) {
            (Source::Conversation, Some(source)) => (&source.root, source.dir.as_path()),
            _ => (&self.records, self.records_dir.as_path()),
        };
        match root.read_file(&file.rel, u64::MAX) {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(refused(&file.rel, dir, "is gone")),
            Err(error) => Err(unreadable(&file.rel, dir, &error)),
        }
    }
}

/// Whether `out` names a zip: it ends in `.zip`, in any case.
fn is_zip_path(out: &Path) -> bool {
    out.to_string_lossy().to_ascii_lowercase().ends_with(".zip")
}

/// The destination a bundle is written to, removed again on failure.
enum Destination {
    Directory(PathBuf),
    Zip {
        path: PathBuf,
        writer: Box<zip::ZipWriter<fs::File>>,
    },
}

impl Destination {
    fn create(out: &Path) -> std::io::Result<Self> {
        if is_zip_path(out) {
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(BUNDLE_FILE_MODE)
                .open(out)?;
            Ok(Self::Zip {
                path: out.to_path_buf(),
                writer: Box::new(zip::ZipWriter::new(file)),
            })
        } else {
            fs::DirBuilder::new().mode(BUNDLE_DIR_MODE).create(out)?;
            Ok(Self::Directory(out.to_path_buf()))
        }
    }

    fn form(&self) -> &'static str {
        match self {
            Self::Directory(_) => "directory",
            Self::Zip { .. } => "zip",
        }
    }

    fn put(&mut self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Directory(root) => write_new_file(root, path, bytes),
            Self::Zip { writer, .. } => {
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(BUNDLE_FILE_MODE);
                writer
                    .start_file(path, options)
                    .map_err(std::io::Error::other)?;
                writer.write_all(bytes)
            }
        }
    }

    /// Writes the zip's central directory. A directory needs nothing more.
    fn finish(self) -> std::io::Result<()> {
        match self {
            Self::Directory(_) => Ok(()),
            Self::Zip { writer, .. } => {
                let mut file = writer.finish().map_err(std::io::Error::other)?;
                file.flush()
            }
        }
    }

    /// Removes everything this export created: the destination was created exclusively, so all of
    /// it is this export's.
    fn remove(self) {
        match self {
            Self::Directory(root) => {
                let _ = fs::remove_dir_all(root);
            }
            Self::Zip { path, writer } => {
                drop(writer);
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Writes `bytes` to the new file `root/rel` at [`BUNDLE_FILE_MODE`], creating its parents at
/// [`BUNDLE_DIR_MODE`]. `rel` is a bundle path this build composed or a normalized zip entry name,
/// so it holds no `..`, and `root` is a directory nothing else writes.
fn write_new_file(root: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(BUNDLE_DIR_MODE)
            .create(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BUNDLE_FILE_MODE)
        .open(&target)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Packs every planned file into `destination`, then `bundle.json`, returning the manifest.
fn write_bundle(
    plan: &ExportPlan,
    destination: &mut Destination,
    session_id: Option<String>,
) -> Result<BundleManifest, String> {
    let mut files = Vec::with_capacity(plan.files.len());
    for file in &plan.files {
        let bytes = plan.read(file).map_err(|error| error.message)?;
        destination
            .put(&file.path, &bytes)
            .map_err(|error| format!("{}: {error}", file.path))?;
        files.push(BundleFile {
            path: file.path.clone(),
            bytes: bytes.len() as u64,
            sha256: murmur_artifact::sha256_hex(&bytes),
        });
    }
    let manifest = BundleManifest::new(
        session_id,
        plan.key_id.clone(),
        plan.conversation
            .as_ref()
            .map(|source| source.pointer.clone()),
        files,
    );
    destination
        .put(BUNDLE_MANIFEST, &manifest.to_bytes())
        .map_err(|error| format!("{BUNDLE_MANIFEST}: {error}"))?;
    Ok(manifest)
}

/// `mur trace export`: packs one ended session's sealed record into a directory or a zip.
///
/// Refuses, before writing anything, a session still running, a destination that exists in any
/// form, and a link or non-regular file among the record's files. On a write failure everything
/// this call created is removed. Exits 0 whatever the record's verdict.
pub(crate) fn run_trace_export(
    session: String,
    out: Option<PathBuf>,
    workdir: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workdir = workdir.unwrap_or_else(|| current_dir.join("workdir"));
    let path = resolve_session(Some(session), &workdir)?;
    let records_dir = records_dir_of(&path);
    let report = verify_records(&records_dir, None)?;
    let session_id = report.session_id.clone().or_else(|| {
        WorkdirRoot::open(&records_dir)
            .ok()
            .and_then(|records| {
                record_seal::read_seal(&records, TRACE_STREAM)
                    .ok()
                    .flatten()
            })
            .map(|seal| seal.session_id)
    });
    let display_id = session_id.clone().unwrap_or_else(|| {
        records_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string())
    });

    let scan = running::running_dir_location()
        .and_then(|dir| running::scan(&dir))
        .map_err(|reason| records_unreadable(&reason))?;
    if let Some(record) = running_record(&scan, &display_id) {
        return Err(CliError::with_hint(
            E_RUN_024,
            format!(
                "session {display_id} is still running (pid {}); nothing was exported",
                record.pid
            ),
            format!("wait for it to end, or stop it with `mur stop {display_id}`, then export it"),
        ));
    }

    let out = out.unwrap_or_else(|| current_dir.join(format!("{display_id}.bundle.zip")));
    let out = std::path::absolute(&out).unwrap_or(out);
    if fs::symlink_metadata(&out).is_ok() {
        return Err(CliError::new(
            E_TRC_008,
            format!("{} already exists; nothing was exported", out.display()),
        ));
    }

    let plan = plan_export(&records_dir)?;
    let cannot_write = |error: String| {
        CliError::new(
            E_TRC_008,
            format!("the bundle {} cannot be written: {error}", out.display()),
        )
    };
    let mut destination =
        Destination::create(&out).map_err(|error| cannot_write(error.to_string()))?;
    let form = destination.form();
    let manifest = match write_bundle(&plan, &mut destination, session_id.clone()) {
        Ok(manifest) => manifest,
        Err(error) => {
            destination.remove();
            return Err(cannot_write(error));
        }
    };
    if let Err(error) = destination.finish() {
        let _ = fs::remove_file(&out);
        return Err(cannot_write(error.to_string()));
    }

    let bytes: u64 = manifest.files.iter().map(|file| file.bytes).sum();
    if json {
        let output = ExportOutput {
            session_id: session_id.as_deref(),
            path: out.display().to_string(),
            form,
            files: manifest.files.len(),
            bytes,
            verdict: report.verdict,
        };
        capsule_runtime::report_println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        capsule_runtime::report_println!("exported session {display_id} to {}", out.display());
        capsule_runtime::report_println!("{} files, {bytes} bytes", manifest.files.len());
        match worst_stream(&report) {
            Some(stream) if report.verdict != Verdict::Verified => {
                capsule_runtime::report_println!(
                    "record: {}: {}",
                    report.verdict.as_str(),
                    stream_finding(stream)
                );
            }
            _ => capsule_runtime::report_println!("record: {}", report.verdict.as_str()),
        }
    }
    Ok(())
}

// ── mur trace verify --bundle ─────────────────────────────────────────────────

/// A bundle opened for checking: its root directory, and the private extraction directory that
/// backs it when the bundle is a zip, removed on drop.
pub(crate) struct OpenedBundle {
    pub(crate) root: PathBuf,
    _extracted: Option<tempfile::TempDir>,
    pub(crate) manifest: BundleManifest,
}

/// The kind of a zip entry, by its unix mode when it carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZipEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// What `--bundle` accepts of one zip entry: its normalized name, or why the archive is refused.
pub(crate) fn check_zip_entry(
    raw_name: &str,
    kind: ZipEntryKind,
    seen: &mut BTreeSet<String>,
) -> Result<Option<String>, String> {
    let name = normalized_entry_name(raw_name).map_err(|error| error.to_string())?;
    match kind {
        ZipEntryKind::Directory => return Ok(None),
        ZipEntryKind::Symlink => {
            return Err(format!("zip entry '{raw_name}' is a symbolic link"));
        }
        ZipEntryKind::Other => {
            return Err(format!("zip entry '{raw_name}' is not a regular file"));
        }
        ZipEntryKind::File => {}
    }
    if name.is_empty() {
        return Err(format!("zip entry '{raw_name}' names no file"));
    }
    if !seen.insert(name.clone()) {
        return Err(format!("zip holds two entries named {name}"));
    }
    Ok(Some(name))
}

fn zip_entry_kind(entry: &zip::read::ZipFile<'_, fs::File>) -> ZipEntryKind {
    match entry.unix_mode().map(|mode| mode & S_IFMT) {
        Some(S_IFLNK) => ZipEntryKind::Symlink,
        _ if entry.is_dir() => ZipEntryKind::Directory,
        None | Some(0) | Some(S_IFREG) => ZipEntryKind::File,
        Some(_) => ZipEntryKind::Other,
    }
}

/// Extracts the zip at `path` into a fresh private directory. Every entry is checked before
/// anything is written.
fn extract_zip(path: &Path) -> Result<tempfile::TempDir, String> {
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("it cannot be read as a zip: {error}"))?;
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("zip entry {index} cannot be read: {error}"))?;
        let raw = entry
            .name()
            .map_err(|error| format!("zip entry {index} has no usable name: {error}"))?
            .into_owned();
        let kind = zip_entry_kind(&entry);
        drop(entry);
        if let Some(name) = check_zip_entry(&raw, kind, &mut seen)? {
            entries.push((raw, name));
        }
    }
    let extracted = tempfile::Builder::new()
        .prefix(".mur-bundle-")
        .tempdir()
        .map_err(|error| format!("no directory to extract it into: {error}"))?;
    fs::set_permissions(
        extracted.path(),
        std::os::unix::fs::PermissionsExt::from_mode(BUNDLE_DIR_MODE),
    )
    .map_err(|error| format!("no directory to extract it into: {error}"))?;
    let cap = max_artifact_decompressed_bytes();
    for (raw, name) in entries {
        let bytes = read_zip_entry_capped(&mut archive, &raw, cap).map_err(|e| e.to_string())?;
        write_new_file(extracted.path(), &name, &bytes)
            .map_err(|error| format!("zip entry '{raw}' cannot be extracted: {error}"))?;
    }
    Ok(extracted)
}

/// Opens the bundle at `path` — a directory, or a zip of one — and reads its `bundle.json`.
/// `Err` carries the reason it is not a usable bundle.
pub(crate) fn open_bundle(path: &Path) -> Result<OpenedBundle, String> {
    let metadata = fs::metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => "it does not exist".to_string(),
        _ => error.to_string(),
    })?;
    let (root, extracted) = if metadata.is_dir() {
        (path.to_path_buf(), None)
    } else if metadata.is_file() {
        let extracted = extract_zip(path)?;
        (extracted.path().to_path_buf(), Some(extracted))
    } else {
        return Err("it is neither a directory nor a file".to_string());
    };
    let opened = WorkdirRoot::open(&root).map_err(|error| error.to_string())?;
    let bytes = match opened.read_file(BUNDLE_MANIFEST, 1 << 26) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err(format!("it holds no {BUNDLE_MANIFEST}")),
        Err(WorkdirError::Refused(_)) => {
            return Err(format!("{BUNDLE_MANIFEST} is not a regular file"))
        }
        Err(error) if error.is_not_found() => return Err(format!("it holds no {BUNDLE_MANIFEST}")),
        Err(error) => return Err(format!("{BUNDLE_MANIFEST} cannot be read: {error}")),
    };
    let manifest = parse_manifest(&bytes)?;
    Ok(OpenedBundle {
        root,
        _extracted: extracted,
        manifest,
    })
}

/// `bundle.json`'s bytes as a [`BundleManifest`] of [`BUNDLE_FORMAT`].
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<BundleManifest, String> {
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("{BUNDLE_MANIFEST} is not JSON: {error}"))?;
    match json.get("format").and_then(serde_json::Value::as_str) {
        Some(BUNDLE_FORMAT) => {}
        Some(other) => {
            return Err(format!(
                "{BUNDLE_MANIFEST} has format {other:?}, not {BUNDLE_FORMAT}"
            ))
        }
        None => {
            return Err(format!(
                "{BUNDLE_MANIFEST} has no format, not {BUNDLE_FORMAT}"
            ))
        }
    }
    serde_json::from_value(json)
        .map_err(|error| format!("{BUNDLE_MANIFEST} is not a {BUNDLE_FORMAT} manifest: {error}"))
}

/// One finding of the bundle checks: a file that does not match its name or the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct BundleFinding {
    pub(crate) path: String,
    /// Always [`Verdict::Changed`].
    pub(crate) verdict: Verdict,
    pub(crate) detail: String,
}

/// `mur trace verify --bundle`'s `bundle` object.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BundleReport {
    /// The bundle path as given, made absolute.
    pub(crate) path: String,
    pub(crate) format: String,
    /// `bundle.json`'s `session_id`.
    pub(crate) session_id: Option<String>,
    pub(crate) exported_at: u64,
    #[serde(skip)]
    pub(crate) exported_by: String,
    /// [`Verdict::Changed`] with any finding, else [`Verdict::Verified`].
    pub(crate) verdict: Verdict,
    /// The number of entries of `bundle.json`'s `files`.
    pub(crate) files: usize,
    pub(crate) findings: Vec<BundleFinding>,
    pub(crate) notes: Vec<String>,
}

/// A regular file found in a bundle, by its path relative to the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldFile {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

/// Every file beneath the bundle root `root`: regular files with their size and SHA-256, and the
/// paths of links and other non-regular entries, none of which is followed.
fn walk_bundle(root: &WorkdirRoot) -> (BTreeMap<String, HeldFile>, Vec<String>) {
    let mut held = BTreeMap::new();
    let mut odd = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        let Ok(Some(entries)) = root.list_dir(&dir) else {
            continue;
        };
        for (name, kind) in entries {
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            match kind {
                EntryKind::Directory => pending.push(rel),
                EntryKind::File => match root.read_file(&rel, u64::MAX) {
                    Ok(Some(bytes)) => {
                        held.insert(
                            rel,
                            HeldFile {
                                bytes: bytes.len() as u64,
                                sha256: murmur_artifact::sha256_hex(&bytes),
                            },
                        );
                    }
                    _ => odd.push(rel),
                },
                EntryKind::Symlink | EntryKind::Other => odd.push(rel),
            }
        }
    }
    odd.sort();
    (held, odd)
}

/// The blob name of `path` when it is a file directly under `blobs/` or
/// `conversations/<record>/<context-id>/blobs/`, whatever the name.
fn blob_file_name(path: &str) -> Option<&str> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [dir, name] if *dir == BLOBS_DIR => Some(name),
        [conversations, _, _, dir, name]
            if *conversations == CONVERSATIONS_DIR && *dir == BLOBS_DIR =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// The blob checks: every blob's bytes against its name, and whether a sealed line names it.
/// `sealed` holds the bytes of `trace.jsonl` and each packed `conversation.jsonl`.
pub(crate) fn check_blobs(
    held: &BTreeMap<String, HeldFile>,
    sealed: &[&[u8]],
    findings: &mut Vec<BundleFinding>,
    notes: &mut Vec<String>,
) {
    for (path, file) in held {
        let Some(name) = blob_file_name(path) else {
            continue;
        };
        if !is_sha256_hex(name) {
            notes.push(format!("{path} is not a blob name"));
            continue;
        }
        if file.sha256 != name {
            findings.push(BundleFinding {
                path: path.clone(),
                verdict: Verdict::Changed,
                detail: format!(
                    "{path}: holds bytes whose sha256 is {}, not the {name} it is named by",
                    file.sha256
                ),
            });
        }
        let named = sealed.iter().any(|bytes| {
            bytes
                .windows(name.len())
                .any(|window| window == name.as_bytes())
        });
        if !named {
            notes.push(format!(
                "{path} is not named by any sealed line of the record"
            ));
        }
    }
}

/// The manifest checks: every listed file held with its size and SHA-256, and every held file
/// listed.
pub(crate) fn check_listed(
    manifest: &BundleManifest,
    held: &BTreeMap<String, HeldFile>,
    findings: &mut Vec<BundleFinding>,
    notes: &mut Vec<String>,
) {
    let mut listed = BTreeSet::new();
    for file in &manifest.files {
        listed.insert(file.path.as_str());
        match held.get(&file.path) {
            None => findings.push(BundleFinding {
                path: file.path.clone(),
                verdict: Verdict::Changed,
                detail: format!(
                    "bundle.json lists {} ({} bytes, sha256 {}), which the bundle does not hold",
                    file.path, file.bytes, file.sha256
                ),
            }),
            Some(found) if found.sha256 != file.sha256 || found.bytes != file.bytes => {
                let mut detail = format!(
                    "bundle.json lists sha256 {} for {}; the file holds {}",
                    file.sha256, file.path, found.sha256
                );
                if found.bytes != file.bytes {
                    detail.push_str(&format!(
                        " ({} bytes listed, {} held)",
                        file.bytes, found.bytes
                    ));
                }
                findings.push(BundleFinding {
                    path: file.path.clone(),
                    verdict: Verdict::Changed,
                    detail,
                });
            }
            Some(_) => {}
        }
    }
    for path in held.keys() {
        if path != BUNDLE_MANIFEST && !listed.contains(path.as_str()) {
            notes.push(format!("{path} is not listed in bundle.json"));
        }
    }
}

/// Every bundle check of `opened` against the record report `report`.
pub(crate) fn check_bundle(
    opened: &OpenedBundle,
    path: &Path,
    report: &RecordReport,
) -> BundleReport {
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    let root = WorkdirRoot::open(&opened.root).ok();
    let (held, odd) = root.as_ref().map(walk_bundle).unwrap_or_default();
    let read = |rel: &str| {
        root.as_ref()
            .and_then(|root| root.read_file(rel, u64::MAX).ok().flatten())
    };
    let mut sealed = Vec::new();
    if let Some(trace) = read(TRACE_STREAM) {
        sealed.push(trace);
    }
    for path in held.keys() {
        let parts: Vec<&str> = path.split('/').collect();
        if let [conversations, _, _, file] = parts.as_slice() {
            if *conversations == CONVERSATIONS_DIR && *file == CONVERSATION_RECORD_FILE {
                if let Some(bytes) = read(path) {
                    sealed.push(bytes);
                }
            }
        }
    }
    let sealed_refs: Vec<&[u8]> = sealed.iter().map(Vec::as_slice).collect();
    check_blobs(&held, &sealed_refs, &mut findings, &mut notes);
    for path in &odd {
        findings.push(BundleFinding {
            path: path.clone(),
            verdict: Verdict::Changed,
            detail: format!("{path} is a symbolic link or not a regular file"),
        });
    }
    check_listed(&opened.manifest, &held, &mut findings, &mut notes);
    if let (Some(listed), Some(traced)) = (&opened.manifest.session_id, &report.session_id) {
        if listed != traced {
            findings.push(BundleFinding {
                path: BUNDLE_MANIFEST.to_string(),
                verdict: Verdict::Changed,
                detail: format!(
                    "bundle.json names session {listed}; trace.jsonl is session {traced}"
                ),
            });
        }
    }
    BundleReport {
        path: path.display().to_string(),
        format: opened.manifest.format.clone(),
        session_id: opened.manifest.session_id.clone(),
        exported_at: opened.manifest.exported_at,
        exported_by: opened.manifest.exported_by.clone(),
        verdict: if findings.is_empty() {
            Verdict::Verified
        } else {
            Verdict::Changed
        },
        files: opened.manifest.files.len(),
        findings,
        notes,
    }
}

/// `mur trace verify`'s `bundle:` block.
fn print_bundle(bundle: &BundleReport) {
    capsule_runtime::report_println!(
        "bundle: {}, {} files listed in bundle.json, exported {} by {}",
        bundle.verdict.as_str(),
        bundle.files,
        bundle.exported_at,
        bundle.exported_by
    );
    for finding in &bundle.findings {
        capsule_runtime::report_println!(
            "  {}  {}  {}",
            finding.path,
            finding.verdict.as_str(),
            finding.detail
        );
    }
    for note in &bundle.notes {
        capsule_runtime::report_println!("  note: {note}");
    }
}

/// `mur trace verify --bundle`: the record inside the bundle checked with `public_key` alone,
/// its attachments, then every file against its name and `bundle.json`.
///
/// Reads nothing but the bundle and the key; a zip is extracted into a private directory removed
/// when this returns.
pub(crate) fn run_trace_verify_bundle(
    bundle: PathBuf,
    public_key: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let path = std::path::absolute(&bundle).unwrap_or(bundle);
    let opened = open_bundle(&path).map_err(|reason| {
        CliError::new(
            E_TRC_009,
            format!("{} is not a usable bundle: {reason}", path.display()),
        )
    })?;
    let key = record_public_key(public_key.as_deref())?;
    let mut report = record_seal::verify_session_records(
        &opened.root,
        key.as_ref(),
        Some(&opened.root.join(CONVERSATIONS_DIR)),
    );
    report.records_dir = path.display().to_string();
    let attachments = trace_attach::attachment_entries(&opened.root, &report);
    let bundle_report = check_bundle(&opened, &path, &report);
    drop(opened);

    let record_verdict = report.verdict;
    let overall = if record_verdict == Verdict::Absent && bundle_report.verdict == Verdict::Verified
    {
        Verdict::Absent
    } else {
        record_seal::worst([record_verdict, bundle_report.verdict])
    };
    let record_finding = worst_stream(&report).map(stream_finding);
    report.verdict = overall;

    if json {
        let output = trace_attach::VerifyOutput {
            report: &report,
            attachments: &attachments,
            bundle: Some(&bundle_report),
        };
        capsule_runtime::report_println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        print_record_report(&report);
        trace_attach::print_attachments(&attachments);
        print_bundle(&bundle_report);
    }

    let session_name = report
        .session_id
        .clone()
        .or_else(|| bundle_report.session_id.clone())
        .unwrap_or_else(|| path.display().to_string());
    let from_record = record_verdict == overall;
    let (subject, finding) = if from_record {
        ("record", record_finding.unwrap_or_default())
    } else {
        (
            "bundle",
            bundle_report
                .findings
                .first()
                .map(|finding| finding.detail.clone())
                .unwrap_or_default(),
        )
    };
    match overall {
        Verdict::Verified => Ok(()),
        Verdict::Changed => Err(CliError::new(
            E_TRC_003,
            format!("the {subject} of session {session_name} has changed: {finding}"),
        )),
        Verdict::Unsealed => Err(CliError::new(
            E_TRC_004,
            format!("the record of session {session_name} is not sealed: {finding}"),
        )),
        Verdict::Unverifiable => Err(CliError::new(
            E_TRC_005,
            format!("the record of session {session_name} cannot be verified: {finding}"),
        )),
        Verdict::Absent => Err(CliError::new(
            E_TRC_001,
            format!("{} holds no trace.jsonl to verify", path.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(entries: &[(&str, &[u8])]) -> BTreeMap<String, HeldFile> {
        entries
            .iter()
            .map(|(path, bytes)| {
                (
                    (*path).to_string(),
                    HeldFile {
                        bytes: bytes.len() as u64,
                        sha256: murmur_artifact::sha256_hex(bytes),
                    },
                )
            })
            .collect()
    }

    fn listed(path: &str, bytes: &[u8]) -> BundleFile {
        BundleFile {
            path: path.to_string(),
            bytes: bytes.len() as u64,
            sha256: murmur_artifact::sha256_hex(bytes),
        }
    }

    #[test]
    fn trace_export_manifest_keys_in_order_and_files_sorted() {
        let manifest = BundleManifest::new(
            Some("ses_1".into()),
            Some("abcd".into()),
            None,
            vec![
                listed("trace.jsonl.seal", b"s"),
                listed("blobs/b", b"b"),
                listed("trace.jsonl", b"t"),
            ],
        );
        let text = String::from_utf8(manifest.to_bytes()).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        let positions: Vec<usize> = [
            "format",
            "session_id",
            "exported_at",
            "exported_by",
            "formats",
            "key_id",
            "conversation",
            "files",
        ]
        .iter()
        .map(|key| text.find(&format!("\"{key}\"")).unwrap())
        .collect();
        assert_eq!(keys.len(), 8);
        let sorted = {
            let mut sorted = positions.clone();
            sorted.sort_unstable();
            sorted
        };
        assert_eq!(positions, sorted, "{text}");
        let paths: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["blobs/b", "trace.jsonl", "trace.jsonl.seal"]);
        assert_eq!(value["format"], BUNDLE_FORMAT);
        assert_eq!(value["conversation"], serde_json::Value::Null);
        assert_eq!(value["formats"]["seal"], record_seal::SEAL_FORMAT);
        assert_eq!(value["formats"]["attachment"], ATTACHMENT_FORMAT);
        assert!(value["exported_by"].as_str().unwrap().starts_with("mur "));
        assert_eq!(parse_manifest(text.as_bytes()).unwrap(), manifest);
    }

    #[test]
    fn trace_export_manifest_format_is_checked() {
        let error = parse_manifest(br#"{"format":"murmur.bundle/9"}"#).unwrap_err();
        assert!(
            error.contains("\"murmur.bundle/9\", not murmur.bundle/1"),
            "{error}"
        );
        let error = parse_manifest(b"not json").unwrap_err();
        assert!(error.contains("is not JSON"), "{error}");
        let error = parse_manifest(b"{}").unwrap_err();
        assert!(error.contains("has no format"), "{error}");
    }

    #[test]
    fn trace_export_zip_entry_checks_refuse() {
        let mut seen = BTreeSet::new();
        assert_eq!(
            check_zip_entry("./trace.jsonl", ZipEntryKind::File, &mut seen).unwrap(),
            Some("trace.jsonl".to_string())
        );
        let duplicate = check_zip_entry("trace.jsonl", ZipEntryKind::File, &mut seen).unwrap_err();
        assert!(
            duplicate.contains("two entries named trace.jsonl"),
            "{duplicate}"
        );
        let escape = check_zip_entry("../escape.txt", ZipEntryKind::File, &mut seen).unwrap_err();
        assert!(escape.contains(".."), "{escape}");
        let link = check_zip_entry("blobs/x", ZipEntryKind::Symlink, &mut seen).unwrap_err();
        assert!(link.contains("symbolic link"), "{link}");
        let fifo = check_zip_entry("blobs/y", ZipEntryKind::Other, &mut seen).unwrap_err();
        assert!(fifo.contains("not a regular file"), "{fifo}");
        assert_eq!(
            check_zip_entry("blobs/", ZipEntryKind::Directory, &mut seen).unwrap(),
            None
        );
    }

    #[test]
    fn trace_export_blob_checks() {
        let good: &[u8] = b"good blob";
        let good_name = murmur_artifact::sha256_hex(good);
        let bad_name = murmur_artifact::sha256_hex(b"original");
        let lonely: &[u8] = b"unanchored";
        let lonely_name = murmur_artifact::sha256_hex(lonely);
        let good_path = format!("blobs/{good_name}");
        let bad_path = format!("blobs/{bad_name}");
        let lonely_path = format!("conversations/cap/ctx/blobs/{lonely_name}");
        let files = held(&[
            (good_path.as_str(), good),
            (bad_path.as_str(), b"edited"),
            (lonely_path.as_str(), lonely),
            ("blobs/README", b"x"),
            ("attachments/att_1.json", b"{}"),
        ]);
        let trace = format!("{{\"a\":\"{good_name}\",\"b\":\"{bad_name}\"}}\n");
        let mut findings = Vec::new();
        let mut notes = Vec::new();
        check_blobs(&files, &[trace.as_bytes()], &mut findings, &mut notes);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, bad_path);
        assert_eq!(
            findings[0].detail,
            format!(
                "{bad_path}: holds bytes whose sha256 is {}, not the {bad_name} it is named by",
                murmur_artifact::sha256_hex(b"edited")
            )
        );
        assert_eq!(
            notes,
            [
                "blobs/README is not a blob name".to_string(),
                format!("{lonely_path} is not named by any sealed line of the record"),
            ]
        );
    }

    #[test]
    fn trace_export_listed_and_unlisted_checks() {
        let files = held(&[
            ("trace.jsonl", b"trace"),
            ("attachments/att_1.json", b"edited"),
            ("notes.txt", b"extra"),
            ("bundle.json", b"{}"),
        ]);
        let manifest = BundleManifest::new(
            None,
            None,
            None,
            vec![
                listed("trace.jsonl", b"trace"),
                listed("attachments/att_1.json", b"original"),
                listed("blobs/gone", b"gone"),
            ],
        );
        let mut findings = Vec::new();
        let mut notes = Vec::new();
        check_listed(&manifest, &files, &mut findings, &mut notes);
        let details: Vec<&str> = findings.iter().map(|f| f.detail.as_str()).collect();
        assert_eq!(details.len(), 2, "{details:?}");
        assert!(details[0].starts_with(&format!(
            "bundle.json lists sha256 {} for attachments/att_1.json; the file holds {}",
            murmur_artifact::sha256_hex(b"original"),
            murmur_artifact::sha256_hex(b"edited")
        )));
        assert_eq!(
            details[1],
            format!(
                "bundle.json lists blobs/gone (4 bytes, sha256 {}), which the bundle does not hold",
                murmur_artifact::sha256_hex(b"gone")
            )
        );
        assert_eq!(notes, ["notes.txt is not listed in bundle.json"]);
    }

    #[test]
    fn trace_export_zip_path_is_case_insensitive() {
        assert!(is_zip_path(Path::new("out/x.ZIP")));
        assert!(is_zip_path(Path::new("x.bundle.zip")));
        assert!(!is_zip_path(Path::new("x.bundle")));
    }
}
