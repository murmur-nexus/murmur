//! `murmur-build.json`: what `mur build` records about an artifact it packs.
//!
//! The entry sits at the archive root beside the packed `murmur.yaml`, which stays the author's
//! file byte for byte. Only `mur build` writes it; an authored file of that name is refused at
//! build. An archive without one (packed by hand or by an older `mur`, or a payload fitting no
//! WIT level) carries no label, and readers derive one from its interfaces.

use std::io::Cursor;

use serde_json::Value;
use thiserror::Error;
use zip::ZipArchive;

use crate::zip_guard;

/// The archive name of the entry `mur build` writes.
pub const BUILD_INFO_FILE: &str = "murmur-build.json";

/// The contents of [`BUILD_INFO_FILE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// The newest WIT level, in the building `mur`'s table, that the packed payload fits.
    pub built_at_level: u64,
}

impl BuildInfo {
    /// The entry's bytes: `{"built_at_level":<N>}` and one newline.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!("{{\"built_at_level\":{}}}\n", self.built_at_level)
    }

    /// Read the entry's text. Keys other than `built_at_level` are ignored.
    pub fn from_json(text: &str) -> Result<Self, BuildInfoError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| BuildInfoError(format!("is not JSON ({error})")))?;
        let object = value
            .as_object()
            .ok_or_else(|| BuildInfoError("is not a JSON object".to_string()))?;
        let level = object
            .get("built_at_level")
            .ok_or_else(|| BuildInfoError("sets no built_at_level".to_string()))?;
        let built_at_level = level.as_u64().ok_or_else(|| {
            BuildInfoError(format!(
                "sets built_at_level to {level}, which is not a non-negative integer"
            ))
        })?;
        Ok(Self { built_at_level })
    }
}

/// Why a packed [`BUILD_INFO_FILE`] cannot be read. The text completes "murmur-build.json …".
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct BuildInfoError(pub String);

/// The [`BUILD_INFO_FILE`] packed in a `.mur.zip`: `Ok(None)` when the bytes are not a zip or
/// hold no such entry, an error when the entry is present but unreadable or malformed.
pub fn build_info_from_artifact_bytes(bytes: &[u8]) -> Result<Option<BuildInfo>, BuildInfoError> {
    let Ok(mut archive) = ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(None);
    };
    if archive.index_for_name(BUILD_INFO_FILE).is_none() {
        return Ok(None);
    }
    let text = zip_guard::read_zip_entry_to_string_capped(
        &mut archive,
        BUILD_INFO_FILE,
        zip_guard::max_artifact_decompressed_bytes(),
    )
    .map_err(|error| BuildInfoError(format!("cannot be read ({error})")))?;
    BuildInfo::from_json(&text).map(Some)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::*;

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = ZipWriter::new(Cursor::new(&mut buf));
            for (name, bytes) in files {
                zip.start_file(*name, SimpleFileOptions::default()).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn build_info_round_trips_its_exact_serialization() {
        let info = BuildInfo { built_at_level: 1 };
        assert_eq!(info.to_json(), "{\"built_at_level\":1}\n");
        let bytes = zip_with(&[(BUILD_INFO_FILE, info.to_json().as_bytes())]);
        assert_eq!(build_info_from_artifact_bytes(&bytes), Ok(Some(info)));
    }

    #[test]
    fn build_info_is_absent_without_the_entry_or_a_zip() {
        let bytes = zip_with(&[("murmur.yaml", b"name: x\n")]);
        assert_eq!(build_info_from_artifact_bytes(&bytes), Ok(None));
        assert_eq!(build_info_from_artifact_bytes(b"not a zip"), Ok(None));
    }

    #[test]
    fn build_info_ignores_unknown_keys_and_refuses_a_bad_level() {
        assert_eq!(
            BuildInfo::from_json("{\"built_at_level\": 3, \"by\": \"mur 9\"}"),
            Ok(BuildInfo { built_at_level: 3 })
        );
        for (text, reason) in [
            ("[1]", "is not a JSON object"),
            ("{}", "sets no built_at_level"),
            (
                "{\"built_at_level\": -1}",
                "sets built_at_level to -1, which is not a non-negative integer",
            ),
            (
                "{\"built_at_level\": \"1\"}",
                "sets built_at_level to \"1\", which is not a non-negative integer",
            ),
            (
                "{\"built_at_level\": 1.5}",
                "sets built_at_level to 1.5, which is not a non-negative integer",
            ),
        ] {
            assert_eq!(
                BuildInfo::from_json(text),
                Err(BuildInfoError(reason.to_string())),
                "{text}"
            );
        }
        let bytes = zip_with(&[(BUILD_INFO_FILE, b"{oops")]);
        assert!(build_info_from_artifact_bytes(&bytes)
            .unwrap_err()
            .0
            .starts_with("is not JSON"));
    }
}
