//! Reading a skill folder in the Agent Skills layout (agentskills.io/specification): a root
//! `SKILL.md` whose YAML front block names, describes and versions the skill, plus the reference
//! files and scripts its body points at.
//!
//! [`collect_skill_folder`] is the one walker for `mur build <folder>` and for a local-source
//! skill whose `source:` is a directory, so both pack and install the same files. A local-source
//! folder read out of an installed package applies the same per-entry rules,
//! [`skill_folder_skips`] and [`classify_skill_folder_root_file`]; change them there, never in a
//! walker. [`check_skill_md`] is the one front-block check for all of them.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    build_info::BUILD_INFO_FILE, manifest_path::MANIFEST_FILENAME, payload_shape::SKILL_MD_ENTRY,
};

/// The longest skill `name` the Agent Skills standard allows, in characters.
pub const SKILL_NAME_MAX_LEN: usize = 64;

/// The longest skill `description` the Agent Skills standard allows, in Unicode scalar values,
/// counted after trimming surrounding whitespace.
pub const SKILL_DESCRIPTION_MAX_CHARS: usize = 1024;

/// The keys of a `SKILL.md` front block that Murmur reads, as [`check_skill_front_block`]
/// accepted them. Every other key (`allowed-tools`, `license`, other `metadata` entries, …) is
/// never parsed into anything Murmur acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFrontBlock {
    /// `name:`, equal to the skill folder's name when there is a folder to compare with.
    pub name: String,
    /// `description:`, trimmed.
    pub description: String,
    /// `metadata.version:`; `None` under [`SkillVersionRule::NotRead`].
    pub version: Option<String>,
}

/// Whether [`check_skill_front_block`] requires and reads `metadata.version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillVersionRule {
    /// `mur build`: the artifact's version is `metadata.version`, a semver string.
    Required,
    /// A local `source:` skill: its version is the capsule entry's `version:`.
    NotRead,
}

/// The front-block rule a `SKILL.md` breaks. `Display` is the rule as the refusal states it;
/// [`SkillFrontBlockError::hint`] says what to change.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SkillFrontBlockError {
    #[error("is not UTF-8 text")]
    NotUtf8,
    #[error(
        "has no front block: the file must open with a line '---', give name, description and \
         metadata.version, and close the block with a line '---'"
    )]
    Missing,
    #[error("front block is not valid YAML: {error}")]
    NotYaml { error: String },
    #[error("front block is not a YAML mapping of keys to values")]
    NotMapping,
    #[error("front block {key} is not a string")]
    NotText { key: &'static str },
    #[error("front block has no name")]
    MissingName,
    #[error("front block name '{name}' may contain only lowercase letters, digits and '-'")]
    NameCharacters { name: String },
    #[error(
        "front block name '{name}' is {len} characters; a skill name is at most \
         {SKILL_NAME_MAX_LEN}"
    )]
    NameTooLong { name: String, len: usize },
    #[error("front block name '{name}' may not {edge} with '-'")]
    NameEdgeHyphen { name: String, edge: &'static str },
    #[error("front block name '{name}' may not contain '--'")]
    NameDoubleHyphen { name: String },
    #[error("front block name '{name}' differs from the folder name '{folder}'")]
    NameDiffersFromFolder { name: String, folder: String },
    #[error("is not inside a folder, so front block name '{name}' has no folder name to match")]
    NoFolder { name: String },
    #[error("front block has no description")]
    MissingDescription,
    #[error(
        "front block description is {chars} characters; a description is at most \
         {SKILL_DESCRIPTION_MAX_CHARS}"
    )]
    DescriptionTooLong { chars: usize },
    #[error("front block has no metadata.version")]
    MissingVersion,
    #[error("front block metadata.version '{version}' is not a semver version: {error}")]
    VersionNotSemver { version: String, error: String },
}

const FRONT_BLOCK_SHAPE: &str =
    "---\nname: <folder name>\ndescription: <what it does and when to use it>\nmetadata:\n  version: \"1.0.0\"\n---";

impl SkillFrontBlockError {
    /// What to change in `SKILL.md`.
    #[must_use]
    pub fn hint(&self) -> String {
        match self {
            Self::NotUtf8 => "save SKILL.md as UTF-8".to_string(),
            Self::Missing | Self::NotMapping => {
                format!("open SKILL.md with a front block:\n{FRONT_BLOCK_SHAPE}")
            }
            Self::NotYaml { .. } => {
                "quote a value that holds ': ', e.g. description: \"Fill forms: use when …\""
                    .to_string()
            }
            Self::NotText { key } => format!("give {key} a string value; quote it if it must be"),
            Self::MissingName => "add name: <the skill folder's name> to the front block".to_string(),
            Self::NameCharacters { .. }
            | Self::NameTooLong { .. }
            | Self::NameEdgeHyphen { .. }
            | Self::NameDoubleHyphen { .. } => format!(
                "a skill name is 1-{SKILL_NAME_MAX_LEN} lowercase letters, digits and single '-' \
                 between them, and the skill folder carries the same name"
            ),
            Self::NameDiffersFromFolder { name, folder } => {
                format!("set name: {folder}, or rename the folder to {name}")
            }
            Self::NoFolder { name } => format!("move SKILL.md into a folder named {name}"),
            Self::MissingDescription => {
                "add description: <what the skill does and when to use it> to the front block"
                    .to_string()
            }
            Self::DescriptionTooLong { .. } => format!(
                "shorten the description to {SKILL_DESCRIPTION_MAX_CHARS} characters; move detail \
                 into the body"
            ),
            Self::MissingVersion => {
                "add the version to the front block:\nmetadata:\n  version: \"1.0.0\"".to_string()
            }
            Self::VersionNotSemver { .. } => {
                "write a full semver version, e.g. version: \"1.2.0\"".to_string()
            }
        }
    }
}

/// Split `text` into its front block and the body after it. A front block exists only when the
/// text (after an optional UTF-8 BOM) opens with a line that is exactly `---` and a later line is
/// exactly `---`; a `\r` before the `\n` is tolerated.
fn split_front_block(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if line_content(first) != "---" {
        return None;
    }
    let block_start = first.len();
    let mut offset = block_start;
    for line in lines {
        if line_content(line) == "---" {
            return Some((&text[block_start..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// A line without its `\n` or `\r\n` ending.
fn line_content(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

/// What the model receives of a `SKILL.md`: the text after the front block's closing line, with
/// leading blank lines removed. A text with no front block is returned whole.
#[must_use]
pub fn skill_md_body(text: &str) -> &str {
    let Some((_, mut body)) = split_front_block(text) else {
        return text;
    };
    while let Some(line) = body.split_inclusive('\n').next() {
        if !line.trim().is_empty() {
            break;
        }
        body = &body[line.len()..];
    }
    body
}

/// Check a `SKILL.md` front block against the Agent Skills standard.
///
/// `folder` is the name of the folder holding `SKILL.md`, which `name` must equal; `None` when
/// the file sits at a package root and so has no folder, which is refused. The block must parse
/// as a YAML mapping; there is no line-by-line reading of a block that does not.
pub fn check_skill_front_block(
    text: &str,
    folder: Option<&str>,
    version_rule: SkillVersionRule,
) -> Result<SkillFrontBlock, SkillFrontBlockError> {
    let (block, _) = split_front_block(text).ok_or(SkillFrontBlockError::Missing)?;
    let value = serde_norway::from_str::<serde_norway::Value>(block).map_err(|error| {
        SkillFrontBlockError::NotYaml {
            error: error.to_string(),
        }
    })?;
    let serde_norway::Value::Mapping(mapping) = value else {
        return Err(SkillFrontBlockError::NotMapping);
    };
    let text_field = |key: &'static str| match mapping.get(key) {
        None | Some(serde_norway::Value::Null) => Ok(None),
        Some(serde_norway::Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(SkillFrontBlockError::NotText { key }),
    };

    let name = text_field("name")?
        .filter(|name| !name.is_empty())
        .ok_or(SkillFrontBlockError::MissingName)?;
    check_skill_name(name)?;
    match folder {
        Some(folder) if folder == name => {}
        Some(folder) => {
            return Err(SkillFrontBlockError::NameDiffersFromFolder {
                name: name.to_string(),
                folder: folder.to_string(),
            })
        }
        None => {
            return Err(SkillFrontBlockError::NoFolder {
                name: name.to_string(),
            })
        }
    }

    let description = text_field("description")?
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .ok_or(SkillFrontBlockError::MissingDescription)?;
    let chars = description.chars().count();
    if chars > SKILL_DESCRIPTION_MAX_CHARS {
        return Err(SkillFrontBlockError::DescriptionTooLong { chars });
    }

    let version = match version_rule {
        SkillVersionRule::NotRead => None,
        SkillVersionRule::Required => Some(metadata_version(&mapping)?),
    };
    Ok(SkillFrontBlock {
        name: name.to_string(),
        description: description.to_string(),
        version,
    })
}

/// The skill name rule, which is stricter than the artifact name rule: at most
/// [`SKILL_NAME_MAX_LEN`] characters and no `--`.
fn check_skill_name(name: &str) -> Result<(), SkillFrontBlockError> {
    let owned = || name.to_string();
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(SkillFrontBlockError::NameCharacters { name: owned() });
    }
    if name.len() > SKILL_NAME_MAX_LEN {
        return Err(SkillFrontBlockError::NameTooLong {
            name: owned(),
            len: name.len(),
        });
    }
    if name.starts_with('-') {
        return Err(SkillFrontBlockError::NameEdgeHyphen {
            name: owned(),
            edge: "start",
        });
    }
    if name.ends_with('-') {
        return Err(SkillFrontBlockError::NameEdgeHyphen {
            name: owned(),
            edge: "end",
        });
    }
    if name.contains("--") {
        return Err(SkillFrontBlockError::NameDoubleHyphen { name: owned() });
    }
    Ok(())
}

fn metadata_version(mapping: &serde_norway::Mapping) -> Result<String, SkillFrontBlockError> {
    let version = match mapping.get("metadata") {
        None | Some(serde_norway::Value::Null) => None,
        Some(serde_norway::Value::Mapping(metadata)) => metadata.get("version"),
        Some(_) => return Err(SkillFrontBlockError::NotText { key: "metadata" }),
    };
    let version = match version {
        None | Some(serde_norway::Value::Null) => return Err(SkillFrontBlockError::MissingVersion),
        Some(serde_norway::Value::String(version)) => version,
        Some(_) => {
            return Err(SkillFrontBlockError::NotText {
                key: "metadata.version",
            })
        }
    };
    semver::Version::parse(version).map_err(|error| SkillFrontBlockError::VersionNotSemver {
        version: version.clone(),
        error: error.to_string(),
    })?;
    Ok(version.clone())
}

/// Check the `SKILL.md` at `path` (as errors name it), whose bytes are `bytes`, by
/// [`check_skill_front_block`].
pub fn check_skill_md(
    path: &Path,
    bytes: &[u8],
    folder: Option<&str>,
    version_rule: SkillVersionRule,
) -> Result<SkillFrontBlock, SkillFolderError> {
    let front_block = |error| SkillFolderError::FrontBlock {
        path: path.to_path_buf(),
        error,
    };
    let text = std::str::from_utf8(bytes).map_err(|_| front_block(SkillFrontBlockError::NotUtf8))?;
    check_skill_front_block(text, folder, version_rule).map_err(front_block)
}

/// The files of a skill folder, as [`collect_skill_folder`] found them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFolder {
    /// The root `SKILL.md`.
    pub skill_md: PathBuf,
    /// The last component of the folder's canonical path, which the front block's `name` must
    /// equal; `None` for a filesystem root.
    pub folder_name: Option<String>,
    /// Every other regular file: the `/`-separated path relative to the folder, and the file's
    /// path on disk. Sorted by relative path.
    pub supporting_files: Vec<(String, PathBuf)>,
}

#[derive(Debug, Error)]
pub enum SkillFolderError {
    #[error("SKILL.md not found in {}", .dir.display())]
    MissingSkillMd { dir: PathBuf },
    #[error("{}: a skill's file is named {SKILL_MD_ENTRY}, exactly", .path.display())]
    MisspelledSkillMd { path: PathBuf },
    #[error("{}: a skill folder holds no murmur.yaml beside {SKILL_MD_ENTRY}", .path.display())]
    ManifestBeside { path: PathBuf },
    #[error("{}: {error}", .path.display())]
    FrontBlock {
        path: PathBuf,
        error: SkillFrontBlockError,
    },
    #[error("{} is a symlink; a skill folder packs regular files only", .path.display())]
    Symlink { path: PathBuf },
    #[error("{} packs as the archive entry '{BUILD_INFO_FILE}', which is reserved for mur build", .path.display())]
    ReservedBuildInfo { path: PathBuf },
    #[error("cannot read {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl SkillFolderError {
    /// Whether the folder breaks the Agent Skills standard itself (`E-BLD-009`), as opposed to
    /// holding something no artifact packs or being unreadable.
    #[must_use]
    pub fn breaks_standard(&self) -> bool {
        matches!(
            self,
            Self::MisspelledSkillMd { .. } | Self::ManifestBeside { .. } | Self::FrontBlock { .. }
        )
    }

    /// What to change; `None` for [`SkillFolderError::MissingSkillMd`] and
    /// [`SkillFolderError::Io`].
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        let hint = match self {
            Self::MisspelledSkillMd { path } => {
                format!("rename {} to {SKILL_MD_ENTRY}", file_name_of(path))
            }
            Self::ManifestBeside { .. } => "a skill's name, description and version go in the \
                                            SKILL.md front block; delete murmur.yaml"
                .to_string(),
            Self::FrontBlock { error, .. } => error.hint(),
            Self::Symlink { .. } => "replace the link with the file it points to, or move it out \
                                     of the skill folder"
                .to_string(),
            Self::ReservedBuildInfo { .. } => {
                "remove it from the skill — mur build writes murmur-build.json itself".to_string()
            }
            Self::MissingSkillMd { .. } | Self::Io { .. } => return None,
        };
        Some(hint)
    }
}

/// Whether the skill-folder walk leaves out an entry named `file_name`, at any depth: a name
/// starting with `.` (`.git/`, `.DS_Store`) or ending `.mur.zip` (any casing).
#[must_use]
pub fn skill_folder_skips(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.to_lowercase().ends_with(".mur.zip")
}

/// Whether a root file named `file_name` is a spelling of `skill.md`, exact or not. `mur build`
/// takes the skill path for a folder holding one.
#[must_use]
pub fn is_skill_md_spelling(file_name: &str) -> bool {
    file_name.to_lowercase() == SKILL_MD_ENTRY.to_lowercase()
}

/// What a regular file at the root of a skill folder is to the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillFolderRootFile {
    /// Exactly [`SKILL_MD_ENTRY`]: the payload.
    SkillMd,
    /// Any other casing of `skill.md`: the folder is refused.
    MisspelledSkillMd,
    /// `murmur.yaml` in any casing: the folder is refused.
    Manifest,
    /// `murmur-build.json`, reserved for `mur build`: the folder is refused.
    ReservedBuildInfo,
    /// Anything else: a supporting file.
    Supporting,
}

/// Classify a regular file at the root of a skill folder by its name.
#[must_use]
pub fn classify_skill_folder_root_file(file_name: &str) -> SkillFolderRootFile {
    if file_name == SKILL_MD_ENTRY {
        SkillFolderRootFile::SkillMd
    } else if is_skill_md_spelling(file_name) {
        SkillFolderRootFile::MisspelledSkillMd
    } else if file_name.to_lowercase() == MANIFEST_FILENAME {
        SkillFolderRootFile::Manifest
    } else if file_name == BUILD_INFO_FILE {
        SkillFolderRootFile::ReservedBuildInfo
    } else {
        SkillFolderRootFile::Supporting
    }
}

/// The refusal for a root file `classify_skill_folder_root_file` does not accept, at `path`.
#[must_use]
pub fn skill_folder_root_refusal(
    class: SkillFolderRootFile,
    path: PathBuf,
) -> Option<SkillFolderError> {
    match class {
        SkillFolderRootFile::MisspelledSkillMd => Some(SkillFolderError::MisspelledSkillMd { path }),
        SkillFolderRootFile::Manifest => Some(SkillFolderError::ManifestBeside { path }),
        SkillFolderRootFile::ReservedBuildInfo => {
            Some(SkillFolderError::ReservedBuildInfo { path })
        }
        SkillFolderRootFile::SkillMd | SkillFolderRootFile::Supporting => None,
    }
}

/// Walk a skill folder.
///
/// Recursive. Skipped: every entry whose name starts with `.` (at any depth — `.git/`,
/// `.DS_Store`) and every file ending `.mur.zip`. The root `SKILL.md` is the payload and every
/// other regular file a supporting file; a `SKILL.md` or `murmur.yaml` below the root is an
/// ordinary supporting file. Refused: a folder with no root `SKILL.md`, a root file spelling
/// `skill.md` any other way, a root `murmur.yaml` in any casing, a symlink anywhere, and a root
/// `murmur-build.json`. The front block is not read here; see [`check_skill_md`].
pub fn collect_skill_folder(dir: &Path) -> Result<SkillFolder, SkillFolderError> {
    let mut skill_md: Option<PathBuf> = None;
    let mut supporting_files = Vec::new();

    for (file_name, path) in read_dir_sorted(dir)? {
        let file_type = file_type_of(&path)?;
        if file_type.is_dir() {
            walk_supporting(&path, &file_name, &mut supporting_files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let class = classify_skill_folder_root_file(&file_name);
        if let Some(refusal) = skill_folder_root_refusal(class, path.clone()) {
            return Err(refusal);
        }
        match class {
            SkillFolderRootFile::SkillMd => skill_md = Some(path),
            _ => supporting_files.push((file_name, path)),
        }
    }

    let skill_md = skill_md.ok_or_else(|| SkillFolderError::MissingSkillMd {
        dir: dir.to_path_buf(),
    })?;
    let folder_name = fs::canonicalize(dir)
        .map_err(|source| SkillFolderError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    supporting_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(SkillFolder {
        skill_md,
        folder_name,
        supporting_files,
    })
}

fn walk_supporting(
    dir: &Path,
    relative: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), SkillFolderError> {
    for (file_name, path) in read_dir_sorted(dir)? {
        let child = format!("{relative}/{file_name}");
        let file_type = file_type_of(&path)?;
        if file_type.is_dir() {
            walk_supporting(&path, &child, out)?;
        } else if file_type.is_file() {
            out.push((child, path));
        }
    }
    Ok(())
}

/// The entries of `dir` that the walk considers, sorted by name: dot-entries and `.mur.zip`
/// files are dropped here, before any symlink check, so a link inside `.git/` is never looked at.
fn read_dir_sorted(dir: &Path) -> Result<Vec<(String, PathBuf)>, SkillFolderError> {
    let io_error = |source| SkillFolderError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if skill_folder_skips(&file_name) {
            continue;
        }
        entries.push((file_name, entry.path()));
    }
    entries.sort();
    Ok(entries)
}

/// The type of `path` without following a link; a link is refused.
fn file_type_of(path: &Path) -> Result<fs::FileType, SkillFolderError> {
    let file_type = fs::symlink_metadata(path)
        .map_err(|source| SkillFolderError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .file_type();
    if file_type.is_symlink() {
        return Err(SkillFolderError::Symlink {
            path: path.to_path_buf(),
        });
    }
    Ok(file_type)
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms; use when a form must be filled.\nmetadata:\n  version: \"1.2.0\"\n---\n# PDF helper\nSee references/forms.md.\n";

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn relative_names(folder: &SkillFolder) -> Vec<&str> {
        folder
            .supporting_files
            .iter()
            .map(|(rel, _)| rel.as_str())
            .collect()
    }

    fn check(text: &str, folder: &str) -> Result<SkillFrontBlock, SkillFrontBlockError> {
        check_skill_front_block(text, Some(folder), SkillVersionRule::Required)
    }

    /// The S1 front block with `name` and `description` replaced.
    fn skill_md(name: &str, description: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {description}\nmetadata:\n  version: \"1.2.0\"\n---\nbody\n"
        )
    }

    fn refusal(text: &str, folder: &str) -> SkillFrontBlockError {
        check(text, folder).unwrap_err()
    }

    #[test]
    fn a_standard_front_block_passes_with_its_three_values() {
        let front = check(PDF_HELPER_SKILL_MD, "pdf-helper").unwrap();
        assert_eq!(
            front,
            SkillFrontBlock {
                name: "pdf-helper".to_string(),
                description: "Fill PDF forms; use when a form must be filled.".to_string(),
                version: Some("1.2.0".to_string()),
            }
        );
    }

    #[test]
    fn keys_murmur_does_not_read_pass_untouched() {
        let text = "\u{feff}---\r\nname: pdf-helper\r\ndescription: >\r\n  Fill forms,\r\n  quickly.\r\nlicense: MIT\r\ncompatibility: Requires python3\r\nallowed-tools: Bash(python3:*) Read\r\nmetadata:\r\n  author: someone\r\n  version: 1.2.0\r\ndisable-model-invocation: false\r\n---\r\nbody\r\n";
        let front = check(text, "pdf-helper").unwrap();
        assert_eq!(front.description, "Fill forms, quickly.");
        assert_eq!(front.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn a_missing_or_unclosed_front_block_is_refused() {
        for text in [
            "# PDF helper\n",
            "",
            "---\nname: pdf-helper\n",
            " ---\nname: x\n---\n",
            "---\nname: x\n...\n",
        ] {
            assert_eq!(
                refusal(text, "pdf-helper"),
                SkillFrontBlockError::Missing,
                "{text:?}"
            );
        }
        assert!(SkillFrontBlockError::Missing.hint().contains("metadata:"));
    }

    #[test]
    fn a_block_that_is_not_yaml_is_refused_with_the_yaml_error_and_a_quoting_hint() {
        let error = refusal(&skill_md("pdf-helper", "Fill: forms"), "pdf-helper");
        assert!(
            matches!(&error, SkillFrontBlockError::NotYaml { error } if error.contains("mapping values")),
            "{error}"
        );
        assert!(error.to_string().starts_with("front block is not valid YAML: "));
        assert!(error.hint().contains("quote a value that holds ': '"));
    }

    #[test]
    fn a_block_that_is_not_a_mapping_is_refused() {
        assert_eq!(
            refusal("---\n- a\n---\n", "a"),
            SkillFrontBlockError::NotMapping
        );
        assert_eq!(refusal("---\n---\n", "a"), SkillFrontBlockError::NotMapping);
    }

    #[test]
    fn name_rules_are_refused_by_name() {
        let cases: [(&str, &str, &str); 7] = [
            (
                "PDF-helper",
                "PDF-helper",
                "front block name 'PDF-helper' may contain only lowercase letters, digits and '-'",
            ),
            (
                "pdf_helper",
                "pdf_helper",
                "front block name 'pdf_helper' may contain only lowercase letters, digits and '-'",
            ),
            (
                "pdf--helper",
                "pdf--helper",
                "front block name 'pdf--helper' may not contain '--'",
            ),
            (
                "-pdf-helper",
                "-pdf-helper",
                "front block name '-pdf-helper' may not start with '-'",
            ),
            (
                "pdf-helper-",
                "pdf-helper-",
                "front block name 'pdf-helper-' may not end with '-'",
            ),
            (
                "other-name",
                "pdf-helper",
                "front block name 'other-name' differs from the folder name 'pdf-helper'",
            ),
            ("''", "pdf-helper", "front block has no name"),
        ];
        for (name, folder, message) in cases {
            assert_eq!(
                refusal(&skill_md(name, "d"), folder).to_string(),
                message,
                "{name}"
            );
        }
        let long = "a".repeat(SKILL_NAME_MAX_LEN + 1);
        assert_eq!(
            refusal(&skill_md(&long, "d"), &long).to_string(),
            format!("front block name '{long}' is 65 characters; a skill name is at most 64")
        );
        let differs = refusal(&skill_md("other-name", "d"), "pdf-helper");
        assert_eq!(
            differs.hint(),
            "set name: pdf-helper, or rename the folder to other-name"
        );
    }

    #[test]
    fn a_64_character_name_passes() {
        let name = format!("{}-b", "a".repeat(SKILL_NAME_MAX_LEN - 2));
        assert_eq!(name.len(), 64);
        assert_eq!(check(&skill_md(&name, "d"), &name).unwrap().name, name);
    }

    #[test]
    fn a_name_without_a_folder_is_refused() {
        let error = check_skill_front_block(
            PDF_HELPER_SKILL_MD,
            None,
            SkillVersionRule::Required,
        )
        .unwrap_err();
        assert_eq!(
            error,
            SkillFrontBlockError::NoFolder {
                name: "pdf-helper".to_string()
            }
        );
        assert_eq!(error.hint(), "move SKILL.md into a folder named pdf-helper");
    }

    #[test]
    fn a_non_string_name_or_description_is_refused() {
        assert_eq!(
            refusal(&skill_md("12", "d"), "12"),
            SkillFrontBlockError::NotText { key: "name" }
        );
        assert_eq!(
            refusal(&skill_md("pdf-helper", "[a]"), "pdf-helper"),
            SkillFrontBlockError::NotText { key: "description" }
        );
    }

    #[test]
    fn description_rules_are_refused_by_name() {
        let no_description =
            "---\nname: pdf-helper\nmetadata:\n  version: \"1.2.0\"\n---\nbody\n";
        assert_eq!(
            refusal(no_description, "pdf-helper").to_string(),
            "front block has no description"
        );
        assert_eq!(
            refusal(&skill_md("pdf-helper", "'   '"), "pdf-helper"),
            SkillFrontBlockError::MissingDescription
        );
        let over = "d".repeat(SKILL_DESCRIPTION_MAX_CHARS + 1);
        assert_eq!(
            refusal(&skill_md("pdf-helper", &over), "pdf-helper").to_string(),
            "front block description is 1025 characters; a description is at most 1024"
        );
    }

    #[test]
    fn a_1024_character_description_passes_counting_scalar_values_after_trimming() {
        let exact = "é".repeat(SKILL_DESCRIPTION_MAX_CHARS);
        let quoted = format!("'  {exact}  '");
        let front = check(&skill_md("pdf-helper", &quoted), "pdf-helper").unwrap();
        assert_eq!(front.description, exact);
    }

    #[test]
    fn version_rules_are_refused_by_name() {
        let no_metadata = "---\nname: pdf-helper\ndescription: d\n---\n";
        assert_eq!(
            refusal(no_metadata, "pdf-helper").to_string(),
            "front block has no metadata.version"
        );
        let no_version = "---\nname: pdf-helper\ndescription: d\nmetadata:\n  author: x\n---\n";
        assert_eq!(
            refusal(no_version, "pdf-helper"),
            SkillFrontBlockError::MissingVersion
        );
        let short = "---\nname: pdf-helper\ndescription: d\nmetadata:\n  version: \"1.2\"\n---\n";
        let error = refusal(short, "pdf-helper");
        assert!(
            error
                .to_string()
                .starts_with("front block metadata.version '1.2' is not a semver version: "),
            "{error}"
        );
        let number = "---\nname: pdf-helper\ndescription: d\nmetadata:\n  version: 1.2\n---\n";
        assert_eq!(
            refusal(number, "pdf-helper"),
            SkillFrontBlockError::NotText {
                key: "metadata.version"
            }
        );
    }

    #[test]
    fn a_local_source_needs_no_version() {
        let no_metadata = "---\nname: pdf-helper\ndescription: d\n---\n";
        let front =
            check_skill_front_block(no_metadata, Some("pdf-helper"), SkillVersionRule::NotRead)
                .unwrap();
        assert_eq!(front.version, None);
    }

    #[test]
    fn check_skill_md_names_the_file_and_refuses_non_utf8() {
        let path = Path::new("/x/pdf-helper/SKILL.md");
        let error = check_skill_md(
            path,
            skill_md("PDF-Helper", "d").as_bytes(),
            Some("pdf-helper"),
            SkillVersionRule::Required,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "/x/pdf-helper/SKILL.md: front block name 'PDF-Helper' may contain only lowercase \
             letters, digits and '-'"
        );
        assert!(error.breaks_standard());
        let error = check_skill_md(path, &[0xff], None, SkillVersionRule::Required).unwrap_err();
        assert_eq!(error.to_string(), "/x/pdf-helper/SKILL.md: is not UTF-8 text");
    }

    #[test]
    fn body_is_the_text_after_the_closing_line_without_leading_blank_lines() {
        assert_eq!(
            skill_md_body(PDF_HELPER_SKILL_MD),
            "# PDF helper\nSee references/forms.md.\n"
        );
        assert_eq!(
            skill_md_body("---\nname: x\n---\n\n  \r\n  indented\n"),
            "  indented\n"
        );
        assert_eq!(skill_md_body("---\r\nname: x\r\n---\r\nbody\r\n"), "body\r\n");
        assert_eq!(skill_md_body("---\nname: x\n---"), "");
        assert_eq!(skill_md_body("# No block\n---\n"), "# No block\n---\n");
    }

    #[test]
    fn root_files_classify_by_exact_name() {
        use SkillFolderRootFile as F;
        for (name, class) in [
            ("SKILL.md", F::SkillMd),
            ("skill.md", F::MisspelledSkillMd),
            ("Skill.md", F::MisspelledSkillMd),
            ("SKILL.MD", F::MisspelledSkillMd),
            ("murmur.yaml", F::Manifest),
            ("Murmur.YAML", F::Manifest),
            (BUILD_INFO_FILE, F::ReservedBuildInfo),
            ("README.md", F::Supporting),
        ] {
            assert_eq!(classify_skill_folder_root_file(name), class, "{name}");
        }
    }

    #[test]
    fn walk_collects_supporting_files_recursively_and_sorted() {
        let base = tempdir().unwrap();
        let dir = base.path().join("pdf-helper");
        write(&dir, "SKILL.md", PDF_HELPER_SKILL_MD);
        write(&dir, "scripts/fill.py", "print(1)\n");
        write(&dir, "references/forms.md", "# Forms\n");
        write(&dir, "references/deep/a.txt", "a");
        write(&dir, "references/SKILL.md", "nested");
        write(&dir, "references/murmur.yaml", "nested is a file\n");
        write(&dir, "README.md", "readme");

        let folder = collect_skill_folder(&dir).unwrap();
        assert_eq!(folder.skill_md, dir.join("SKILL.md"));
        assert_eq!(folder.folder_name.as_deref(), Some("pdf-helper"));
        assert_eq!(
            relative_names(&folder),
            [
                "README.md",
                "references/SKILL.md",
                "references/deep/a.txt",
                "references/forms.md",
                "references/murmur.yaml",
                "scripts/fill.py"
            ]
        );
    }

    #[test]
    fn the_folder_name_is_the_canonical_last_component() {
        let base = tempdir().unwrap();
        let dir = base.path().join("pdf-helper");
        write(&dir, "SKILL.md", PDF_HELPER_SKILL_MD);
        let folder = collect_skill_folder(&dir.join(".")).unwrap();
        assert_eq!(folder.folder_name.as_deref(), Some("pdf-helper"));
    }

    #[test]
    fn walk_skips_dot_entries_and_artifacts_at_any_depth() {
        let dir = tempdir().unwrap();
        write(dir.path(), "SKILL.md", "# s\n");
        write(dir.path(), ".git/HEAD", "ref");
        write(dir.path(), ".DS_Store", "x");
        write(dir.path(), "references/.hidden", "x");
        write(dir.path(), "old-0.0.1.mur.zip", "zip");
        write(dir.path(), "references/nested.MUR.ZIP", "zip");
        write(dir.path(), "references/kept.md", "kept");
        #[cfg(unix)]
        std::os::unix::fs::symlink("HEAD", dir.path().join(".git/link")).unwrap();

        let folder = collect_skill_folder(dir.path()).unwrap();
        assert_eq!(relative_names(&folder), ["references/kept.md"]);
    }

    #[test]
    fn walk_refuses_a_misspelled_skill_md_alone_or_beside_skill_md() {
        for (files, misspelled) in [
            (&["skill.md"][..], "skill.md"),
            (&["Skill.md"][..], "Skill.md"),
            (&["SKILL.md", "skill.md"][..], "skill.md"),
        ] {
            let dir = tempdir().unwrap();
            for file in files {
                write(dir.path(), file, PDF_HELPER_SKILL_MD);
            }
            let err = collect_skill_folder(dir.path()).unwrap_err();
            assert!(
                matches!(&err, SkillFolderError::MisspelledSkillMd { path } if path.ends_with(misspelled)),
                "{err}"
            );
            assert!(err.breaks_standard());
            assert!(
                err.to_string().ends_with("a skill's file is named SKILL.md, exactly"),
                "{err}"
            );
            assert_eq!(
                err.hint().unwrap(),
                format!("rename {misspelled} to SKILL.md")
            );
        }
    }

    #[test]
    fn walk_refuses_a_manifest_beside_skill_md() {
        for manifest in ["murmur.yaml", "Murmur.yaml"] {
            let dir = tempdir().unwrap();
            write(dir.path(), "SKILL.md", PDF_HELPER_SKILL_MD);
            write(dir.path(), manifest, "name: pdf-helper\n");
            let err = collect_skill_folder(dir.path()).unwrap_err();
            assert!(
                matches!(&err, SkillFolderError::ManifestBeside { path } if path.ends_with(manifest)),
                "{err}"
            );
            assert_eq!(
                err.hint().unwrap(),
                "a skill's name, description and version go in the SKILL.md front block; \
                 delete murmur.yaml"
            );
        }
    }

    #[test]
    fn walk_refuses_a_folder_without_skill_md() {
        let dir = tempdir().unwrap();
        write(dir.path(), "README.md", "readme");
        write(dir.path(), "sub/SKILL.md", "nested does not count");
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(
            matches!(&err, SkillFolderError::MissingSkillMd { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("SKILL.md not found in"), "{err}");
        assert!(!err.breaks_standard());
    }

    #[cfg(unix)]
    #[test]
    fn walk_refuses_a_symlink_anywhere() {
        let dir = tempdir().unwrap();
        write(dir.path(), "SKILL.md", "a");
        write(dir.path(), "references/real.md", "real");
        std::os::unix::fs::symlink("real.md", dir.path().join("references/link.md")).unwrap();
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(
            matches!(&err, SkillFolderError::Symlink { path } if path.ends_with("references/link.md")),
            "{err}"
        );
        assert!(!err.breaks_standard());
    }

    #[cfg(unix)]
    #[test]
    fn walk_refuses_a_symlinked_skill_md() {
        let dir = tempdir().unwrap();
        write(dir.path(), "real.md", "a");
        std::os::unix::fs::symlink("real.md", dir.path().join("SKILL.md")).unwrap();
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(matches!(&err, SkillFolderError::Symlink { .. }), "{err}");
    }

    #[test]
    fn walk_refuses_a_root_build_info_file() {
        let dir = tempdir().unwrap();
        write(dir.path(), "SKILL.md", "a");
        write(dir.path(), BUILD_INFO_FILE, "{}");
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(
            matches!(&err, SkillFolderError::ReservedBuildInfo { path } if path.ends_with(BUILD_INFO_FILE)),
            "{err}"
        );
    }

    #[test]
    fn walk_reports_an_unreadable_folder() {
        let dir = tempdir().unwrap();
        let err = collect_skill_folder(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(&err, SkillFolderError::Io { .. }), "{err}");
    }
}
