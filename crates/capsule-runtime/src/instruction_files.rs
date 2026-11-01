//! `inference.instruction_files`: the work-folder files a manifest names, read once at staging
//! and appended to the system prompt as labelled blocks after the manifest's own prompt.
//!
//! Every entry resolves beneath the accessible work folder through
//! [`resolve_relpath_beneath_root`], following links only while they stay inside it, and is
//! opened with [`open_no_follow`]. Nothing here goes through [`crate::workdir_fs::WorkdirRoot`]:
//! it refuses every link and latches the refusal, which would fail the launch over a
//! `CLAUDE.md -> AGENTS.md` link that is the ordinary way a repository serves both names.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use murmur_artifact::ExportMode;
use serde::Serialize;

use crate::resource_plane::{
    io_to_resource_error, open_no_follow, resolve_relpath_beneath_root, DeclaredExport,
    ResourceError, SymlinkPolicy,
};

/// The most bytes of one listed file that reach the system prompt; the rest is cut at the last
/// whole UTF-8 character and the block's header says so. Codex's `project_doc_max_bytes`
/// defaults to 32 KiB, so a file Codex loads whole loads whole here; Claude Code's own guidance
/// is under 200 lines per file.
pub(crate) const INSTRUCTION_FILE_MAX_BYTES: u64 = 32 * 1024;

/// The most directory entries the search for other files with the listed names visits.
pub(crate) const NESTED_WALK_MAX_ENTRIES: usize = 10_000;

/// The most of those other files `MURMUR.md` names; the rest are counted.
pub(crate) const NESTED_LISTED_MAX: usize = 32;

/// What staging found for `inference.instruction_files`, held on the session for every task of
/// the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstructionFiles {
    /// One per manifest entry, in manifest order.
    pub(crate) entries: Vec<InstructionFileEntry>,
    /// Other files in the work folder whose names match a listed entry's file name.
    pub(crate) nested: NestedFiles,
}

/// One manifest entry and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstructionFileEntry {
    /// The entry as the manifest wrote it.
    pub(crate) path: String,
    pub(crate) status: InstructionFileStatus,
}

/// What one entry resolved to. Exactly one per entry; [`Self::wire_name`] is its trace spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstructionFileStatus {
    /// `read`: the file contributes a block.
    Read(ReadFile),
    /// `missing`: no such path, or a link whose target does not exist.
    Missing,
    /// `same_file`: an earlier entry, `same_as` as the manifest wrote it, resolved to the same
    /// file.
    SameFile { same_as: String },
    /// `outside_work_folder`: the path, through a link, resolves outside the work folder.
    OutsideWorkFolder,
    /// `not_a_file`: a directory, FIFO, socket or device.
    NotAFile,
    /// `not_utf8`: the bytes are not UTF-8, apart from a final character the cut split.
    NotUtf8,
    /// `empty`: no character other than whitespace.
    Empty,
    /// `unreadable`: any other failure, with its text.
    Unreadable { error: String },
}

impl InstructionFileStatus {
    pub(crate) fn wire_name(&self) -> &'static str {
        match self {
            Self::Read(_) => "read",
            Self::Missing => "missing",
            Self::SameFile { .. } => "same_file",
            Self::OutsideWorkFolder => "outside_work_folder",
            Self::NotAFile => "not_a_file",
            Self::NotUtf8 => "not_utf8",
            Self::Empty => "empty",
            Self::Unreadable { .. } => "unreadable",
        }
    }

    /// How `MURMUR.md` says why an entry is not in the system prompt. `None` for the two statuses
    /// that are: `read`, and `same_file`, which is named on its first entry.
    pub(crate) fn absence_reason(&self) -> Option<&'static str> {
        match self {
            Self::Read(_) | Self::SameFile { .. } => None,
            Self::Missing => Some("missing"),
            Self::OutsideWorkFolder => Some("outside the work folder"),
            Self::NotAFile => Some("not a file"),
            Self::NotUtf8 => Some("not UTF-8"),
            Self::Empty => Some("empty"),
            Self::Unreadable { .. } => Some("unreadable"),
        }
    }
}

/// A listed file that was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadFile {
    /// The canonical path relative to the work folder, `/`-separated.
    pub(crate) resolved: String,
    /// The file's size, from `fstat` on the open descriptor.
    pub(crate) bytes: u64,
    /// The text as read: at most [`INSTRUCTION_FILE_MAX_BYTES`], ending on a whole character.
    pub(crate) text: String,
}

impl ReadFile {
    pub(crate) fn included_bytes(&self) -> u64 {
        self.text.len() as u64
    }

    pub(crate) fn truncated(&self) -> bool {
        self.included_bytes() < self.bytes
    }

    /// `first <included> of <bytes> bytes` for a cut file, else `None`.
    pub(crate) fn cut_note(&self) -> Option<String> {
        self.truncated()
            .then(|| format!("first {} of {} bytes", self.included_bytes(), self.bytes))
    }
}

/// Files elsewhere in the work folder with a listed entry's file name, which are not loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NestedFiles {
    /// At most [`NESTED_LISTED_MAX`] work-folder-relative paths, in walk order.
    pub(crate) listed: Vec<String>,
    /// How many more were found.
    pub(crate) more: usize,
    /// Whether the walk stopped at [`NESTED_WALK_MAX_ENTRIES`] with entries left to visit.
    pub(crate) stopped: bool,
}

/// One element of `session_start.instruction_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct InstructionFileRecord {
    path: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    included_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
    /// Of the included text, not of the whole file.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    same_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl InstructionFiles {
    /// Reads every entry of `entries` beneath `work_folder`, then walks the work folder for other
    /// files with the same names.
    pub(crate) fn read(work_folder: &Path, entries: &[String]) -> Self {
        let export = DeclaredExport {
            declared_root: ".".to_string(),
            root: work_folder.to_path_buf(),
            mode: ExportMode::ReadOnly,
            max_bytes: INSTRUCTION_FILE_MAX_BYTES,
        };
        let mut first_by_resolved: BTreeMap<String, String> = BTreeMap::new();
        let read_entries = entries
            .iter()
            .map(|path| {
                let status = read_entry(&export, path, &mut first_by_resolved);
                InstructionFileEntry {
                    path: path.clone(),
                    status,
                }
            })
            .collect::<Vec<_>>();

        let names: BTreeSet<OsString> = entries
            .iter()
            .filter_map(|entry| Path::new(entry).file_name().map(OsString::from))
            .collect();
        let excluded: BTreeSet<String> = entries
            .iter()
            .map(|entry| lower(entry))
            .chain(read_entries.iter().filter_map(|entry| match &entry.status {
                InstructionFileStatus::Read(file) => Some(file.resolved.clone()),
                _ => None,
            }))
            .collect();
        let nested = walk_nested(work_folder, &names, &excluded, NESTED_WALK_MAX_ENTRIES);

        Self {
            entries: read_entries,
            nested,
        }
    }

    /// Every later entry naming the same file as `first`, as the manifest wrote them.
    fn aliases_of(&self, first: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.status {
                InstructionFileStatus::SameFile { same_as } if same_as == first => {
                    Some(entry.path.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// The `read` entries with their later aliases, in manifest order.
    pub(crate) fn read_files(&self) -> Vec<(&str, &ReadFile, Vec<&str>)> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.status {
                InstructionFileStatus::Read(file) => {
                    Some((entry.path.as_str(), file, self.aliases_of(&entry.path)))
                }
                _ => None,
            })
            .collect()
    }

    /// One block per `read` entry, in manifest order, each `[Instruction file: <entry>…]` and the
    /// text with its trailing line endings removed.
    pub(crate) fn blocks(&self) -> Vec<String> {
        self.read_files()
            .into_iter()
            .map(|(path, file, aliases)| {
                let mut header = format!("[Instruction file: {path}");
                if !aliases.is_empty() {
                    header.push_str(&format!(" (also listed as {})", aliases.join(", ")));
                }
                if let Some(cut) = file.cut_note() {
                    header.push_str(&format!(", {cut}"));
                }
                let text = file.text.trim_end_matches(['\n', '\r']);
                format!("{header}]\n{text}")
            })
            .collect()
    }

    /// `session_start.instruction_files`: one record per entry, in manifest order.
    pub(crate) fn trace_records(&self) -> Vec<InstructionFileRecord> {
        self.entries
            .iter()
            .map(|entry| {
                let mut record = InstructionFileRecord {
                    path: entry.path.clone(),
                    status: entry.status.wire_name(),
                    resolved: None,
                    bytes: None,
                    included_bytes: None,
                    truncated: None,
                    sha256: None,
                    same_as: None,
                    error: None,
                };
                match &entry.status {
                    InstructionFileStatus::Read(file) => {
                        record.resolved = Some(file.resolved.clone());
                        record.bytes = Some(file.bytes);
                        record.included_bytes = Some(file.included_bytes());
                        record.truncated = Some(file.truncated());
                        record.sha256 = Some(murmur_artifact::sha256_hex(file.text.as_bytes()));
                    }
                    InstructionFileStatus::SameFile { same_as } => {
                        record.same_as = Some(same_as.clone());
                    }
                    InstructionFileStatus::Unreadable { error } => {
                        record.error = Some(error.clone());
                    }
                    _ => {}
                }
                record
            })
            .collect()
    }
}

/// The operator text the prompt builders receive: `operator_prompt` (the manifest's resolved
/// prompt, or `--system-prompt`), then a blank line, then the blocks of `files` joined by blank
/// lines. With no operator prompt, or an empty one, the blocks alone. With no block, exactly
/// `operator_prompt`.
pub(crate) fn compose_system_prompt(
    operator_prompt: Option<&str>,
    files: Option<&InstructionFiles>,
) -> Option<String> {
    let blocks = files.map(InstructionFiles::blocks).unwrap_or_default();
    if blocks.is_empty() {
        return operator_prompt.map(str::to_string);
    }
    let blocks = blocks.join("\n\n");
    Some(match operator_prompt.filter(|prompt| !prompt.is_empty()) {
        Some(prompt) => format!("{prompt}\n\n{blocks}"),
        None => blocks,
    })
}

/// `entry`'s `Normal` components joined by `/`: `./AGENTS.md` and `AGENTS.md` name one path, and
/// [`resolve_relpath_beneath_root`] refuses a `.` component. The manifest refused `..` and
/// absolute entries before staging.
fn lower(entry: &str) -> String {
    Path::new(entry)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn read_entry(
    export: &DeclaredExport,
    entry: &str,
    first_by_resolved: &mut BTreeMap<String, String>,
) -> InstructionFileStatus {
    let lowered = lower(entry);
    // `.` alone names the work folder itself.
    if lowered.is_empty() {
        return InstructionFileStatus::NotAFile;
    }
    let (target, resolved) =
        match resolve_relpath_beneath_root(export, &lowered, SymlinkPolicy::FollowWithinRoot) {
            Ok(found) => found,
            Err(error) => return status_of(error),
        };
    if let Some(first) = first_by_resolved.get(&resolved) {
        return InstructionFileStatus::SameFile {
            same_as: first.clone(),
        };
    }
    first_by_resolved.insert(resolved.clone(), entry.to_string());
    match read_bounded(&target, INSTRUCTION_FILE_MAX_BYTES) {
        Ok((bytes, text)) => InstructionFileStatus::Read(ReadFile {
            resolved,
            bytes,
            text,
        }),
        Err(status) => status,
    }
}

fn status_of(error: ResourceError) -> InstructionFileStatus {
    match error {
        ResourceError::NotFound => InstructionFileStatus::Missing,
        ResourceError::OutsideRoot => InstructionFileStatus::OutsideWorkFolder,
        ResourceError::NotARegularFile => InstructionFileStatus::NotAFile,
        other => InstructionFileStatus::Unreadable {
            error: other.message(),
        },
    }
}

/// Opens the canonical `target` without following a link and reads at most `max_bytes` of it,
/// returning the file's size and its text cut at the last whole character at or below the limit.
fn read_bounded(target: &Path, max_bytes: u64) -> Result<(u64, String), InstructionFileStatus> {
    let (file, metadata) =
        open_no_follow(target).map_err(|e| status_of(io_to_resource_error(&e)))?;
    if !metadata.is_file() {
        return Err(InstructionFileStatus::NotAFile);
    }
    let size = metadata.len();
    let mut bytes = Vec::with_capacity(size.min(max_bytes) as usize);
    file.take(size.min(max_bytes))
        .read_to_end(&mut bytes)
        .map_err(|e| status_of(io_to_resource_error(&e)))?;
    let cut = (bytes.len() as u64) < size;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        // Only a cut file may end mid-character, and only at its very end.
        Err(error) if cut && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).map_err(|_| InstructionFileStatus::NotUtf8)?
        }
        Err(_) => return Err(InstructionFileStatus::NotUtf8),
    };
    if text.trim().is_empty() {
        return Err(InstructionFileStatus::Empty);
    }
    Ok((size, text))
}

/// Walks `work_folder` for files or links named like one of `names`, at a relative path not in
/// `excluded`. No link is followed or descended into, no directory named `.git` is entered, each
/// directory is visited in name order, and the walk stops after `max_entries` entries.
fn walk_nested(
    work_folder: &Path,
    names: &BTreeSet<OsString>,
    excluded: &BTreeSet<String>,
    max_entries: usize,
) -> NestedFiles {
    let mut nested = NestedFiles::default();
    if names.is_empty() {
        return nested;
    }
    let mut visited = 0usize;
    // Each frame is one directory's remaining children, reversed so `pop` yields name order.
    let mut stack: Vec<(String, Vec<(OsString, std::fs::FileType)>)> = Vec::new();
    let open_dir = |path: &Path, remaining: usize| -> Vec<(OsString, std::fs::FileType)> {
        let Ok(reader) = std::fs::read_dir(path) else {
            return Vec::new();
        };
        // One past the budget is enough to know the walk stopped with entries left.
        let mut children: Vec<_> = reader
            .flatten()
            .take(remaining.saturating_add(1))
            .filter_map(|entry| Some((entry.file_name(), entry.file_type().ok()?)))
            .collect();
        children.sort_by(|a, b| b.0.cmp(&a.0));
        children
    };
    stack.push((String::new(), open_dir(work_folder, max_entries)));
    while let Some((prefix, children)) = stack.last_mut() {
        let Some((name, file_type)) = children.pop() else {
            stack.pop();
            continue;
        };
        if visited == max_entries {
            nested.stopped = true;
            break;
        }
        visited += 1;
        let relative = if prefix.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{prefix}/{}", name.to_string_lossy())
        };
        if file_type.is_dir() {
            if name != ".git" {
                let path: PathBuf = work_folder.join(&relative);
                let children = open_dir(&path, max_entries - visited);
                stack.push((relative, children));
            }
            continue;
        }
        if (file_type.is_file() || file_type.is_symlink())
            && names.contains(&name)
            && !excluded.contains(&relative)
        {
            if nested.listed.len() < NESTED_LISTED_MAX {
                nested.listed.push(relative);
            } else {
                nested.more += 1;
            }
        }
    }
    nested
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::TempDir;

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn statuses(files: &InstructionFiles) -> Vec<(&str, &'static str)> {
        files
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.status.wire_name()))
            .collect()
    }

    #[test]
    fn instruction_files_read_one_file_into_one_block() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "Use tabs.\n\n").unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert_eq!(statuses(&files), vec![("AGENTS.md", "read")]);
        assert_eq!(
            files.blocks(),
            vec!["[Instruction file: AGENTS.md]\nUse tabs.".to_string()]
        );
        let record = serde_json::to_value(&files.trace_records()[0]).unwrap();
        assert_eq!(
            record,
            serde_json::json!({
                "path": "AGENTS.md",
                "status": "read",
                "resolved": "AGENTS.md",
                "bytes": 11,
                "included_bytes": 11,
                "truncated": false,
                "sha256": murmur_artifact::sha256_hex(b"Use tabs.\n\n"),
            })
        );
    }

    #[test]
    fn instruction_files_crlf_endings_are_removed_from_the_end_only() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "a\r\nb\r\n").unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["./AGENTS.md"]));
        assert_eq!(
            files.blocks(),
            vec!["[Instruction file: ./AGENTS.md]\na\r\nb".to_string()]
        );
    }

    #[test]
    fn instruction_files_compose_places_blocks_after_the_operator_prompt() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("A.md"), "alpha").unwrap();
        fs::write(dir.path().join("B.md"), "beta").unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["A.md", "B.md"]));
        assert_eq!(
            compose_system_prompt(Some("MANIFEST"), Some(&files)).as_deref(),
            Some("MANIFEST\n\n[Instruction file: A.md]\nalpha\n\n[Instruction file: B.md]\nbeta")
        );
        let blocks_alone = "[Instruction file: A.md]\nalpha\n\n[Instruction file: B.md]\nbeta";
        assert_eq!(
            compose_system_prompt(None, Some(&files)).as_deref(),
            Some(blocks_alone)
        );
        assert_eq!(
            compose_system_prompt(Some(""), Some(&files)).as_deref(),
            Some(blocks_alone)
        );
    }

    /// With no block read, the operator text is passed through exactly, `None` and `""` included.
    #[test]
    fn instruction_files_compose_without_a_block_changes_nothing() {
        let dir = TempDir::new().unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        for prompt in [None, Some(""), Some("MANIFEST")] {
            assert_eq!(
                compose_system_prompt(prompt, Some(&files)).as_deref(),
                prompt
            );
            assert_eq!(compose_system_prompt(prompt, None).as_deref(), prompt);
        }
    }

    #[test]
    fn instruction_files_missing_entry_is_recorded_and_skipped() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
        symlink(dir.path().join("nowhere"), dir.path().join("DANGLING.md")).unwrap();
        let files = InstructionFiles::read(
            dir.path(),
            &entries(&["AGENTS.md", "CLAUDE.md", "DANGLING.md", "AGENTS.md/x"]),
        );
        assert_eq!(
            statuses(&files),
            vec![
                ("AGENTS.md", "read"),
                ("CLAUDE.md", "missing"),
                ("DANGLING.md", "missing"),
                ("AGENTS.md/x", "missing"),
            ]
        );
        assert_eq!(files.blocks().len(), 1);
        assert_eq!(
            serde_json::to_value(&files.trace_records()[1]).unwrap(),
            serde_json::json!({"path": "CLAUDE.md", "status": "missing"})
        );
    }

    #[test]
    fn instruction_files_one_file_under_two_names_is_read_once() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "shared").unwrap();
        symlink("CLAUDE.md", dir.path().join("AGENTS.md")).unwrap();
        let files = InstructionFiles::read(
            dir.path(),
            &entries(&["AGENTS.md", "CLAUDE.md", "./CLAUDE.md"]),
        );
        assert_eq!(
            statuses(&files),
            vec![
                ("AGENTS.md", "read"),
                ("CLAUDE.md", "same_file"),
                ("./CLAUDE.md", "same_file"),
            ]
        );
        assert_eq!(
            files.blocks(),
            vec![
                "[Instruction file: AGENTS.md (also listed as CLAUDE.md, ./CLAUDE.md)]\nshared"
                    .to_string()
            ]
        );
        let records = files.trace_records();
        let first = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(first["resolved"], "CLAUDE.md");
        assert_eq!(
            serde_json::to_value(&records[1]).unwrap(),
            serde_json::json!({"path": "CLAUDE.md", "status": "same_file", "same_as": "AGENTS.md"})
        );
    }

    #[test]
    fn instruction_files_links_out_of_the_work_folder_are_not_read() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret.md"), "OUTSIDE-MARKER").unwrap();
        fs::write(outside.path().join("AGENTS.md"), "OUTSIDE-MARKER").unwrap();
        let dir = TempDir::new().unwrap();
        symlink(
            outside.path().join("secret.md"),
            dir.path().join("AGENTS.md"),
        )
        .unwrap();
        symlink(outside.path(), dir.path().join("docs")).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md", "docs/AGENTS.md"]));
        assert_eq!(
            statuses(&files),
            vec![
                ("AGENTS.md", "outside_work_folder"),
                ("docs/AGENTS.md", "outside_work_folder"),
            ]
        );
        assert!(files.blocks().is_empty());
    }

    #[test]
    fn instruction_files_over_the_limit_are_cut_and_say_so() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "a".repeat(40_000)).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        let block = &files.blocks()[0];
        let (header, body) = block.split_once('\n').unwrap();
        assert_eq!(
            header,
            "[Instruction file: AGENTS.md, first 32768 of 40000 bytes]"
        );
        assert_eq!(body.len(), 32_768);
        let record = serde_json::to_value(&files.trace_records()[0]).unwrap();
        assert_eq!(record["bytes"], 40_000);
        assert_eq!(record["included_bytes"], 32_768);
        assert_eq!(record["truncated"], true);
    }

    /// A character straddling the limit is left out whole, and the header counts what remains.
    #[test]
    fn instruction_files_cut_lands_before_a_split_character() {
        let dir = TempDir::new().unwrap();
        let mut text = "a".repeat(32_766);
        text.push('€'); // three bytes: 32766..32769
        text.push_str(&"b".repeat(100));
        fs::write(dir.path().join("AGENTS.md"), &text).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        let InstructionFileStatus::Read(file) = &files.entries[0].status else {
            panic!("{:?}", files.entries[0].status);
        };
        assert_eq!(file.included_bytes(), 32_766);
        assert_eq!(file.bytes, text.len() as u64);
        assert!(files.blocks()[0].starts_with(&format!(
            "[Instruction file: AGENTS.md, first 32766 of {} bytes]\n",
            text.len()
        )));
    }

    #[test]
    fn instruction_files_cut_and_alias_share_one_header() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "a".repeat(51_200)).unwrap();
        symlink("AGENTS.md", dir.path().join("CLAUDE.md")).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md", "CLAUDE.md"]));
        assert!(files.blocks()[0].starts_with(
            "[Instruction file: AGENTS.md (also listed as CLAUDE.md), first 32768 of 51200 \
             bytes]\n"
        ));
    }

    #[test]
    fn instruction_files_not_a_file_covers_directories_and_fifos() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("AGENTS.md")).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(dir.path().join("PIPE.md"))
            .status()
            .unwrap()
            .success());
        let files =
            InstructionFiles::read(dir.path(), &entries(&["AGENTS.md", "PIPE.md", ".", "./"]));
        assert_eq!(
            statuses(&files),
            vec![
                ("AGENTS.md", "not_a_file"),
                ("PIPE.md", "not_a_file"),
                (".", "not_a_file"),
                ("./", "not_a_file"),
            ]
        );
    }

    #[test]
    fn instruction_files_not_utf8_and_empty_are_skipped() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("BIN.md"), [b'a', 0xff, b'b']).unwrap();
        fs::write(dir.path().join("BLANK.md"), " \n\t\r\n").unwrap();
        fs::write(dir.path().join("ZERO.md"), "").unwrap();
        let files =
            InstructionFiles::read(dir.path(), &entries(&["BIN.md", "BLANK.md", "ZERO.md"]));
        assert_eq!(
            statuses(&files),
            vec![
                ("BIN.md", "not_utf8"),
                ("BLANK.md", "empty"),
                ("ZERO.md", "empty")
            ]
        );
        assert!(files.blocks().is_empty());
    }

    /// Invalid UTF-8 before the limit is `not_utf8` even in a file the cut shortens.
    #[test]
    fn instruction_files_not_utf8_in_a_cut_file_is_not_excused() {
        let dir = TempDir::new().unwrap();
        let mut bytes = vec![b'a'; 40_000];
        bytes[10] = 0xff;
        fs::write(dir.path().join("AGENTS.md"), bytes).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert_eq!(statuses(&files), vec![("AGENTS.md", "not_utf8")]);
    }

    #[test]
    fn instruction_files_unreadable_carries_the_error() {
        // Root reads a mode-000 file, so there is nothing to observe.
        if crate::murmur_home::current_euid() == 0 {
            return;
        }
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(statuses(&files), vec![("AGENTS.md", "unreadable")]);
        let record = serde_json::to_value(&files.trace_records()[0]).unwrap();
        assert!(
            record["error"]
                .as_str()
                .unwrap()
                .contains("ermission denied"),
            "{record}"
        );
    }

    #[test]
    fn instruction_files_nested_names_are_found_but_not_read() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        fs::create_dir_all(dir.path().join("src/api")).unwrap();
        fs::write(dir.path().join("src/api/AGENTS.md"), "nested").unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/AGENTS.md"), "git").unwrap();
        fs::create_dir_all(dir.path().join("web")).unwrap();
        fs::write(dir.path().join("web/CLAUDE.md"), "not listed").unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert_eq!(files.nested.listed, vec!["src/api/AGENTS.md".to_string()]);
        assert_eq!(files.nested.more, 0);
        assert!(!files.nested.stopped);
        assert_eq!(files.blocks(), vec!["[Instruction file: AGENTS.md]\nroot"]);
    }

    /// A listed name that only a link reaches is still a match, and a linked directory is never
    /// entered, inside the work folder or out of it.
    #[test]
    fn instruction_files_nested_walk_descends_no_link() {
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("AGENTS.md"), "outside").unwrap();
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/AGENTS.md"), "x").unwrap();
        symlink(dir.path().join("a"), dir.path().join("b")).unwrap();
        symlink(outside.path(), dir.path().join("out")).unwrap();
        fs::create_dir_all(dir.path().join("c")).unwrap();
        symlink("../a/AGENTS.md", dir.path().join("c/AGENTS.md")).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert_eq!(
            files.nested.listed,
            vec!["a/AGENTS.md".to_string(), "c/AGENTS.md".to_string()]
        );
    }

    /// A read entry's canonical path is not reported as another copy of itself.
    #[test]
    fn instruction_files_nested_walk_skips_the_resolved_target() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/AGENTS.md"), "x").unwrap();
        symlink("docs/AGENTS.md", dir.path().join("AGENTS.md")).unwrap();
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert!(files.nested.listed.is_empty(), "{:?}", files.nested);
    }

    #[test]
    fn instruction_files_nested_matches_past_the_listed_max_are_counted() {
        let dir = TempDir::new().unwrap();
        for i in 0..(NESTED_LISTED_MAX + 3) {
            let sub = dir.path().join(format!("d{i:03}"));
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("AGENTS.md"), "x").unwrap();
        }
        let files = InstructionFiles::read(dir.path(), &entries(&["AGENTS.md"]));
        assert_eq!(files.nested.listed.len(), NESTED_LISTED_MAX);
        assert_eq!(files.nested.listed[0], "d000/AGENTS.md");
        assert_eq!(files.nested.more, 3);
        assert!(!files.nested.stopped);
    }

    #[test]
    fn instruction_files_nested_walk_stops_at_its_bound() {
        let dir = TempDir::new().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("f{i}")), "x").unwrap();
        }
        fs::create_dir_all(dir.path().join("z")).unwrap();
        fs::write(dir.path().join("z/AGENTS.md"), "x").unwrap();
        let names: BTreeSet<OsString> = [OsString::from("AGENTS.md")].into();
        let stopped = walk_nested(dir.path(), &names, &BTreeSet::new(), 5);
        assert!(stopped.stopped);
        assert!(stopped.listed.is_empty());
        let whole = walk_nested(dir.path(), &names, &BTreeSet::new(), 12);
        assert!(!whole.stopped);
        assert_eq!(whole.listed, vec!["z/AGENTS.md".to_string()]);
    }
}
