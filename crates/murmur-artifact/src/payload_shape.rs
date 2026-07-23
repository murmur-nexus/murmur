//! The shared payload-shape contract for `.mur.zip` artifacts.
//!
//! This module is the single place that answers "what counts as a structurally valid wasm /
//! native / skill payload inside a `.mur.zip`": which entry name holds the payload, and — for
//! wasm artifacts, where the name is not fixed — which of the archive's root `*.wasm` entries
//! is selected, or which error the caller must report.
//!
//! The rules are expressed in two layers so that both zip-backed readers and (later) the
//! pre-pack file list in `mur build` can use them:
//!
//! * a pure core over entry-name strings — [`is_root_wasm_candidate`],
//!   [`root_wasm_candidates`], [`select_root_wasm`] and [`select_root_wasm_from_entries`] —
//!   which touches no archive and no filesystem, and
//! * a thin archive-walking wrapper, [`select_root_wasm_in_archive`], for callers that already
//!   hold an open [`ZipArchive`].
//!
//! Path safety is *not* reimplemented here: root-entry validity is judged with
//! [`crate::zip_guard::sanitize_entry_path`].

use std::io::{Read, Seek};

use thiserror::Error;
use zip::ZipArchive;

use crate::zip_guard::sanitize_entry_path;

/// The preferred root wasm entry name: when an artifact contains this entry it is always the
/// selected payload, regardless of how many other root `*.wasm` entries exist.
pub const CAPSULE_WASM_ENTRY: &str = "capsule.wasm";

/// The fixed payload entry name for a skill (static) artifact.
pub const SKILL_MD_ENTRY: &str = "skill.md";

/// The directory holding a native artifact's compiled binary.
pub const NATIVE_BIN_DIR: &str = "bin";

/// File extension (including the dot) of a wasm payload entry.
pub const WASM_EXTENSION: &str = ".wasm";

/// The payload entry name for a native artifact: `bin/<artifact_name>`.
#[must_use]
pub fn native_binary_entry(artifact_name: &str) -> String {
    format!("{NATIVE_BIN_DIR}/{artifact_name}")
}

/// Why a set of archive entries does not describe a valid wasm payload shape.
///
/// The `Display` text of each variant is the user-facing message reported by every caller, so
/// it is part of this module's contract.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PayloadShapeError {
    /// No entry qualified as a root `*.wasm` candidate.
    #[error("missing root .wasm file (expected capsule.wasm or one root *.wasm)")]
    MissingRootWasm,
    /// More than one root `*.wasm` candidate and none of them is `capsule.wasm`.
    /// `names` is sorted ascending.
    #[error("multiple root .wasm files found: {}", names.join(", "))]
    MultipleRootWasm { names: Vec<String> },
}

/// Is `raw_name` a valid root `*.wasm` payload candidate?
///
/// A candidate must end in `.wasm`, have a safe path (per
/// [`sanitize_entry_path`](crate::zip_guard::sanitize_entry_path)), and be a single top-level
/// path component — entries nested in a subdirectory or carrying a leading `/` are rejected
/// (both of which `sanitize_entry_path` would otherwise normalize away, but which must never
/// be treated as "the root wasm file").
#[must_use]
pub fn is_root_wasm_candidate(raw_name: &str) -> bool {
    if !raw_name.ends_with(WASM_EXTENSION) {
        return false;
    }

    let Ok(sanitized) = sanitize_entry_path(raw_name) else {
        return false;
    };

    !raw_name.starts_with('/') && sanitized.components().count() == 1
}

/// Filter `entry_names` down to the root `*.wasm` payload candidates, preserving input order.
pub fn root_wasm_candidates<I, S>(entry_names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    entry_names
        .into_iter()
        .filter(|name| is_root_wasm_candidate(name.as_ref()))
        .map(|name| name.as_ref().to_string())
        .collect()
}

/// Select the wasm payload from an already-filtered candidate list.
///
/// `capsule.wasm` always wins when present; otherwise exactly one candidate is required.
/// This is the pure core of the wasm payload-shape rule: no archive, no filesystem.
pub fn select_root_wasm<S: AsRef<str>>(candidates: &[S]) -> Result<String, PayloadShapeError> {
    if candidates
        .iter()
        .any(|name| name.as_ref() == CAPSULE_WASM_ENTRY)
    {
        return Ok(CAPSULE_WASM_ENTRY.to_string());
    }

    match candidates.len() {
        1 => Ok(candidates[0].as_ref().to_string()),
        0 => Err(PayloadShapeError::MissingRootWasm),
        _ => {
            let mut names: Vec<String> = candidates
                .iter()
                .map(|name| name.as_ref().to_string())
                .collect();
            names.sort();
            Err(PayloadShapeError::MultipleRootWasm { names })
        }
    }
}

/// Select the wasm payload from a list of *all* entry names (filtering, then selecting).
///
/// This is the entry point for callers that hold a plain list of relative paths — e.g. the
/// file list `mur build` assembles before packing.
pub fn select_root_wasm_from_entries<I, S>(entry_names: I) -> Result<String, PayloadShapeError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    select_root_wasm(&root_wasm_candidates(entry_names))
}

/// Select the wasm payload from an open `.mur.zip` archive.
///
/// Thin archive-walking layer over [`select_root_wasm_from_entries`]: entries whose metadata
/// cannot be read are skipped, exactly as they are ignored during candidate filtering.
pub fn select_root_wasm_in_archive<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<String, PayloadShapeError> {
    let mut entry_names = Vec::with_capacity(archive.len());
    for idx in 0..archive.len() {
        let Ok(file) = archive.by_index(idx) else {
            continue;
        };
        entry_names.push(file.name().to_string());
    }

    select_root_wasm_from_entries(entry_names)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    fn archive_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = SimpleFileOptions::default();
            for (name, bytes) in files {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn open(bytes: &[u8]) -> ZipArchive<std::io::Cursor<&[u8]>> {
        ZipArchive::new(std::io::Cursor::new(bytes)).unwrap()
    }

    #[test]
    fn zero_candidates_is_missing_root_wasm() {
        let err = select_root_wasm_from_entries(["murmur.yaml", "README.md"]).unwrap_err();
        assert_eq!(err, PayloadShapeError::MissingRootWasm);
        assert_eq!(
            err.to_string(),
            "missing root .wasm file (expected capsule.wasm or one root *.wasm)"
        );
    }

    #[test]
    fn exactly_one_candidate_is_selected() {
        let selected = select_root_wasm_from_entries(["murmur.yaml", "tool.wasm"]).unwrap();
        assert_eq!(selected, "tool.wasm");
    }

    #[test]
    fn multiple_candidates_report_sorted_comma_joined_names() {
        let err =
            select_root_wasm_from_entries(["zeta.wasm", "alpha.wasm", "murmur.yaml", "mid.wasm"])
                .unwrap_err();
        assert_eq!(
            err,
            PayloadShapeError::MultipleRootWasm {
                names: vec![
                    "alpha.wasm".to_string(),
                    "mid.wasm".to_string(),
                    "zeta.wasm".to_string(),
                ]
            }
        );
        assert_eq!(
            err.to_string(),
            "multiple root .wasm files found: alpha.wasm, mid.wasm, zeta.wasm"
        );
    }

    #[test]
    fn capsule_wasm_is_preferred_among_multiple_candidates() {
        let selected =
            select_root_wasm_from_entries(["tool.wasm", "capsule.wasm", "other.wasm"]).unwrap();
        assert_eq!(selected, CAPSULE_WASM_ENTRY);
    }

    #[test]
    fn nested_subdirectory_wasm_is_not_a_root_candidate() {
        assert!(!is_root_wasm_candidate("sub/capsule.wasm"));

        let err = select_root_wasm_from_entries(["sub/capsule.wasm", "murmur.yaml"]).unwrap_err();
        assert_eq!(err, PayloadShapeError::MissingRootWasm);
    }

    #[test]
    fn traversal_and_absolute_entries_are_not_root_candidates() {
        assert!(!is_root_wasm_candidate("../../evil.wasm"));
        assert!(!is_root_wasm_candidate("/capsule.wasm"));

        let err = select_root_wasm_from_entries(["../../evil.wasm", "/capsule.wasm"]).unwrap_err();
        assert_eq!(err, PayloadShapeError::MissingRootWasm);
    }

    #[test]
    fn non_wasm_entries_are_not_root_candidates() {
        assert!(!is_root_wasm_candidate("capsule.wat"));
        assert!(!is_root_wasm_candidate("skill.md"));
        assert!(is_root_wasm_candidate("capsule.wasm"));
    }

    #[test]
    fn root_wasm_candidates_preserves_input_order() {
        let candidates =
            root_wasm_candidates(["zeta.wasm", "murmur.yaml", "alpha.wasm", "sub/nested.wasm"]);
        assert_eq!(candidates, vec!["zeta.wasm", "alpha.wasm"]);
    }

    #[test]
    fn select_root_wasm_in_archive_prefers_capsule_wasm() {
        let bytes = archive_with_files(&[
            ("tool.wasm", b"tool"),
            ("capsule.wasm", b"capsule"),
            ("murmur.yaml", b"name: demo"),
        ]);

        let selected = select_root_wasm_in_archive(&mut open(&bytes)).unwrap();
        assert_eq!(selected, CAPSULE_WASM_ENTRY);
    }

    #[test]
    fn select_root_wasm_in_archive_reports_missing_for_nested_only_wasm() {
        let bytes = archive_with_files(&[("sub/capsule.wasm", b"nested")]);

        let err = select_root_wasm_in_archive(&mut open(&bytes)).unwrap_err();
        assert_eq!(err, PayloadShapeError::MissingRootWasm);
    }

    #[test]
    fn select_root_wasm_in_archive_reports_multiple_sorted() {
        let bytes = archive_with_files(&[("zeta.wasm", b"z"), ("alpha.wasm", b"a")]);

        let err = select_root_wasm_in_archive(&mut open(&bytes)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "multiple root .wasm files found: alpha.wasm, zeta.wasm"
        );
    }

    #[test]
    fn native_binary_entry_is_bin_slash_name() {
        assert_eq!(native_binary_entry("demo"), "bin/demo");
    }
}
