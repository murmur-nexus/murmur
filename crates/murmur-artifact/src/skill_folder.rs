//! Reading a skill folder in the layout Claude Code, Codex and pi share: a root `SKILL.md`
//! whose front block names and describes the skill, plus the reference files and scripts its
//! body points at.
//!
//! [`collect_skill_folder`] is the one walker for a manifest-less `mur build <folder>` and for a
//! local-source skill whose `source:` is a directory, so both pack and install the same files.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{build_info::BUILD_INFO_FILE, manifest_path::MANIFEST_FILENAME};

/// The keys of a `SKILL.md` front block that Murmur reads. Every other key (`allowed-tools`,
/// `license`, `metadata`, …) is ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillFrontBlock {
    /// The `name:` value, trimmed; `None` when absent, empty or not a string.
    pub name: Option<String>,
    /// The `description:` value, trimmed; `None` when absent, empty or not a string.
    pub description: Option<String>,
}

/// Split `text` into its front block and the body after it. A front block exists only when
/// the text (after an optional UTF-8 BOM) opens with a line that is exactly `---` and a later
/// line is exactly `---` or `...`.
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
        let content = line_content(line);
        if content == "---" || content == "..." {
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

/// The `name` and `description` of a `SKILL.md` front block, or `None` when the text has no
/// front block.
///
/// The block is read as a YAML mapping. Hand-written front blocks are often not valid YAML
/// (`description: Fill forms: use when …` carries an unquoted `: `), so when the block does not
/// parse as a mapping the top-level `name:` and `description:` lines are read instead: split at
/// the first `:`, trimmed, with one pair of matching surrounding quotes removed.
#[must_use]
pub fn parse_skill_front_block(text: &str) -> Option<SkillFrontBlock> {
    let (block, _) = split_front_block(text)?;
    let parsed = serde_norway::from_str::<serde_norway::Value>(block)
        .ok()
        .and_then(|value| match value {
            serde_norway::Value::Mapping(mapping) => Some(mapping),
            _ => None,
        });
    let front = match parsed {
        Some(mapping) => {
            let field = |key: &str| {
                mapping
                    .get(key)
                    .and_then(serde_norway::Value::as_str)
                    .and_then(non_empty)
            };
            SkillFrontBlock {
                name: field("name"),
                description: field("description"),
            }
        }
        None => SkillFrontBlock {
            name: top_level_line_value(block, "name"),
            description: top_level_line_value(block, "description"),
        },
    };
    Some(front)
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The value of the first unindented `key: value` line in `block`.
fn top_level_line_value(block: &str, key: &str) -> Option<String> {
    block.lines().find_map(|line| {
        if line.starts_with(char::is_whitespace) {
            return None;
        }
        let (line_key, value) = line.split_once(':')?;
        if line_key.trim_end() != key {
            return None;
        }
        let value = value.trim();
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|quote| {
                value
                    .strip_prefix(*quote)
                    .and_then(|rest| rest.strip_suffix(*quote))
            })
            .unwrap_or(value);
        non_empty(unquoted)
    })
}

/// How a skill describes itself: its front block's `description`, else the first non-empty line
/// after the front block (of the whole text when there is none), trimmed.
#[must_use]
pub fn skill_md_description(text: &str) -> Option<String> {
    let body = match split_front_block(text) {
        Some((_, body)) => {
            if let Some(description) = parse_skill_front_block(text).and_then(|f| f.description) {
                return Some(description);
            }
            body
        }
        None => text.strip_prefix('\u{feff}').unwrap_or(text),
    };
    body.lines().find_map(non_empty)
}

/// The files of a skill folder, as [`collect_skill_folder`] found them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFolder {
    /// The root `skill.md`, in whatever casing the folder spells it. It packs and installs as
    /// `skill.md`.
    pub skill_md: PathBuf,
    /// The root `murmur.yaml` (any casing), when there is one. It is never a supporting file.
    pub manifest: Option<PathBuf>,
    /// Every other regular file: the `/`-separated path relative to the folder, and the file's
    /// path on disk. Sorted by relative path.
    pub supporting_files: Vec<(String, PathBuf)>,
}

#[derive(Debug, Error)]
pub enum SkillFolderError {
    #[error("SKILL.md not found in {} (case-insensitive search found no match)", .dir.display())]
    MissingSkillMd { dir: PathBuf },
    #[error("{} holds both {first} and {second}; a skill has one skill.md", .dir.display())]
    DuplicateSkillMd {
        dir: PathBuf,
        first: String,
        second: String,
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

/// Walk a skill folder.
///
/// Recursive. Skipped: every entry whose name starts with `.` (at any depth — `.git/`,
/// `.DS_Store`) and every file ending `.mur.zip`. The root `skill.md` in any casing is the
/// payload, the root `murmur.yaml` in any casing the manifest, and every other regular file a
/// supporting file. Refused: a folder with no root `skill.md`, one with two spellings of it, a
/// symlink anywhere, and a root `murmur-build.json`.
pub fn collect_skill_folder(dir: &Path) -> Result<SkillFolder, SkillFolderError> {
    let mut skill_md: Option<PathBuf> = None;
    let mut manifest: Option<PathBuf> = None;
    let mut supporting_files = Vec::new();

    for (file_name, path) in read_dir_sorted(dir)? {
        let lower = file_name.to_lowercase();
        let file_type = file_type_of(&path)?;
        if file_type.is_dir() {
            walk_supporting(&path, &file_name, &mut supporting_files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if lower == "skill.md" {
            if let Some(first) = &skill_md {
                return Err(SkillFolderError::DuplicateSkillMd {
                    dir: dir.to_path_buf(),
                    first: display_name(first),
                    second: file_name,
                });
            }
            skill_md = Some(path);
        } else if lower == MANIFEST_FILENAME {
            if manifest.is_none() || file_name == MANIFEST_FILENAME {
                manifest = Some(path);
            }
        } else if file_name == BUILD_INFO_FILE {
            return Err(SkillFolderError::ReservedBuildInfo { path });
        } else {
            supporting_files.push((file_name, path));
        }
    }

    let skill_md = skill_md.ok_or_else(|| SkillFolderError::MissingSkillMd {
        dir: dir.to_path_buf(),
    })?;
    supporting_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(SkillFolder {
        skill_md,
        manifest,
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
        if file_name.starts_with('.') || file_name.to_lowercase().ends_with(".mur.zip") {
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

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms: use when a form must be filled.\n---\n# PDF helper\nSee references/forms.md.\n";

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

    #[test]
    fn front_block_with_unquoted_colon_falls_back_to_lines() {
        let front = parse_skill_front_block(PDF_HELPER_SKILL_MD).unwrap();
        assert_eq!(front.name.as_deref(), Some("pdf-helper"));
        assert_eq!(
            front.description.as_deref(),
            Some("Fill PDF forms: use when a form must be filled.")
        );
    }

    #[test]
    fn front_block_reads_a_folded_description() {
        let text = "---\nname: folded\ndescription: >\n  First line\n  second line.\nlicense: MIT\n---\nbody\n";
        let front = parse_skill_front_block(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("folded"));
        assert_eq!(
            front.description.as_deref(),
            Some("First line second line.")
        );
    }

    #[test]
    fn front_block_ignores_other_keys_and_accepts_dots_and_bom() {
        let text = "\u{feff}---\nallowed-tools: [Read]\nname: 'quoted-name'\nmetadata:\n  name: inner\n...\nbody\n";
        let front = parse_skill_front_block(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("quoted-name"));
        assert_eq!(front.description, None);
    }

    #[test]
    fn line_fallback_strips_one_pair_of_quotes_and_skips_indented_keys() {
        let text = "---\nmetadata:\n  name: inner\nname: \"x: y\"\ndescription: 'Use: when'\nbad: [\n---\n";
        let front = parse_skill_front_block(text).unwrap();
        assert_eq!(front.name.as_deref(), Some("x: y"));
        assert_eq!(front.description.as_deref(), Some("Use: when"));
    }

    #[test]
    fn empty_and_non_string_values_are_none() {
        let front = parse_skill_front_block("---\nname: ''\ndescription: 3\n---\n").unwrap();
        assert_eq!(front, SkillFrontBlock::default());
    }

    #[test]
    fn no_front_block_without_both_fences() {
        assert_eq!(parse_skill_front_block("# Title\n---\n"), None);
        assert_eq!(parse_skill_front_block("---\nname: x\n"), None);
        assert_eq!(parse_skill_front_block(" ---\nname: x\n---\n"), None);
        assert_eq!(parse_skill_front_block(""), None);
    }

    #[test]
    fn crlf_fences_are_recognised() {
        let front = parse_skill_front_block("---\r\nname: crlf\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(front.name.as_deref(), Some("crlf"));
    }

    #[test]
    fn description_prefers_the_front_block() {
        assert_eq!(
            skill_md_description(PDF_HELPER_SKILL_MD).as_deref(),
            Some("Fill PDF forms: use when a form must be filled.")
        );
    }

    #[test]
    fn description_without_front_block_description_is_first_line_after_the_block() {
        assert_eq!(
            skill_md_description("---\nname: x\n---\n\n  # Heading  \nmore\n").as_deref(),
            Some("# Heading")
        );
    }

    #[test]
    fn description_without_front_block_is_first_non_empty_line() {
        assert_eq!(
            skill_md_description("\n\n# My Skill\nDo things.\n").as_deref(),
            Some("# My Skill")
        );
        assert_eq!(skill_md_description("\n  \n"), None);
    }

    #[test]
    fn walk_collects_supporting_files_recursively_and_sorted() {
        let dir = tempdir().unwrap();
        write(dir.path(), "SKILL.md", PDF_HELPER_SKILL_MD);
        write(dir.path(), "scripts/fill.py", "print(1)\n");
        write(dir.path(), "references/forms.md", "# Forms\n");
        write(dir.path(), "references/deep/a.txt", "a");
        write(dir.path(), "README.md", "readme");

        let folder = collect_skill_folder(dir.path()).unwrap();
        assert_eq!(folder.skill_md, dir.path().join("SKILL.md"));
        assert_eq!(folder.manifest, None);
        assert_eq!(
            relative_names(&folder),
            [
                "README.md",
                "references/deep/a.txt",
                "references/forms.md",
                "scripts/fill.py"
            ]
        );
        assert_eq!(
            folder.supporting_files[2].1,
            dir.path().join("references/forms.md")
        );
    }

    #[test]
    fn walk_skips_dot_entries_and_artifacts_at_any_depth() {
        let dir = tempdir().unwrap();
        write(dir.path(), "skill.md", "# s\n");
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
    fn walk_reports_the_manifest_and_never_packs_it() {
        let dir = tempdir().unwrap();
        write(dir.path(), "Skill.MD", "# s\n");
        write(dir.path(), "Murmur.yaml", "name: x\n");
        write(dir.path(), "references/murmur.yaml", "nested is a file\n");

        let folder = collect_skill_folder(dir.path()).unwrap();
        assert_eq!(folder.skill_md, dir.path().join("Skill.MD"));
        assert_eq!(folder.manifest, Some(dir.path().join("Murmur.yaml")));
        assert_eq!(relative_names(&folder), ["references/murmur.yaml"]);
    }

    #[test]
    fn walk_refuses_a_folder_without_skill_md() {
        let dir = tempdir().unwrap();
        write(dir.path(), "README.md", "readme");
        write(dir.path(), "sub/skill.md", "nested does not count");
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(
            matches!(&err, SkillFolderError::MissingSkillMd { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("SKILL.md not found in"), "{err}");
    }

    #[test]
    fn walk_refuses_two_spellings_of_skill_md() {
        let dir = tempdir().unwrap();
        write(dir.path(), "SKILL.md", "a");
        write(dir.path(), "skill.md", "b");
        let err = collect_skill_folder(dir.path()).unwrap_err();
        assert!(
            matches!(&err, SkillFolderError::DuplicateSkillMd { first, second, .. }
                if first == "SKILL.md" && second == "skill.md"),
            "{err}"
        );
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
