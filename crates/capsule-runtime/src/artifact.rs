use murmur_artifact::payload_shape::{
    native_binary_entry, select_root_wasm_in_archive, SKILL_MD_ENTRY,
};
use murmur_artifact::zip_guard;
use murmur_artifact::PACKED_MANIFEST_ENTRY;
use zip::ZipArchive;

use crate::errors::RuntimeError;

pub fn extract_root_wasm(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_root_wasm_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_root_wasm_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    // Which root entry counts as the wasm payload is the shared payload-shape contract; the
    // selector (and its error text) lives in murmur_artifact::payload_shape.
    let selected_name =
        select_root_wasm_in_archive(&mut archive).map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: err.to_string(),
        })?;

    zip_guard::read_zip_entry_capped(&mut archive, &selected_name, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: err.to_string(),
        }
    })
}

/// Extract the native binary from `bin/<artifact_name>` inside a `.mur.zip`.
///
/// Canonical native artifact layout:
///   murmur.yaml            — artifact manifest at zip root
///   bin/<artifact_name>    — compiled binary with executable permissions
pub fn extract_native_binary(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_native_binary_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_native_binary_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    let bin_path = native_binary_entry(artifact_name);
    zip_guard::read_zip_entry_capped(&mut archive, &bin_path, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "failed to read native binary at '{bin_path}' from archive (ensure the binary \
                 is at {bin_path} inside the .mur.zip): {err}"
            ),
        }
    })
}

/// Extract `skill.md` from a skill artifact's `.mur.zip`.
///
/// Canonical skill artifact layout:
///   murmur.yaml      — artifact manifest at zip root
///   skill.md         — guidance content at zip root (primary payload)
pub fn extract_skill_md(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_skill_md_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_skill_md_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    zip_guard::read_zip_entry_capped(&mut archive, SKILL_MD_ENTRY, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "{SKILL_MD_ENTRY} not found or unreadable at archive root (ensure \
                 {SKILL_MD_ENTRY} is at the root of the .mur.zip): {err}"
            ),
        }
    })
}

pub fn extract_manifest_yaml(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<String, RuntimeError> {
    extract_manifest_yaml_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_manifest_yaml_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<String, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    zip_guard::read_zip_entry_to_string_capped(&mut archive, PACKED_MANIFEST_ENTRY, max_bytes)
        .map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "missing or unreadable {PACKED_MANIFEST_ENTRY} in artifact archive: {err}"
            ),
        })
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

    #[test]
    fn prefers_capsule_wasm_when_present() {
        let archive = archive_with_files(&[
            ("tool.wasm", b"tool"),
            ("capsule.wasm", b"capsule"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let wasm = extract_root_wasm("demo", "0.0.1", &archive).unwrap();
        assert_eq!(wasm, b"capsule");
    }

    #[test]
    fn extracts_manifest_yaml_from_archive() {
        let archive = archive_with_files(&[
            (PACKED_MANIFEST_ENTRY, b"name: demo\nversion: 0.1.0\n"),
            ("tool.wasm", b"tool"),
        ]);

        let manifest = extract_manifest_yaml("demo", "0.1.0", &archive).unwrap();
        assert!(manifest.contains("name: demo"));
        assert!(manifest.contains("version: 0.1.0"));
    }

    #[test]
    fn rejects_path_traversal_entry_as_root_wasm_candidate() {
        let archive = archive_with_files(&[
            ("../../evil.wasm", b"evil"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let err = extract_root_wasm("demo", "0.0.1", &archive).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("missing root .wasm file"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_leading_slash_entry_as_root_wasm_candidate() {
        let archive = archive_with_files(&[
            ("/capsule.wasm", b"evil"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let err = extract_root_wasm("demo", "0.0.1", &archive).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("missing root .wasm file"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_manifest_yaml_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[(PACKED_MANIFEST_ENTRY, &big)]);

        let err = extract_manifest_yaml_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_root_wasm_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("capsule.wasm", &big)]);

        let err = extract_root_wasm_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_native_binary_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("bin/demo", &big)]);

        let err = extract_native_binary_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_skill_md_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("skill.md", &big)]);

        let err = extract_skill_md_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn default_ceiling_does_not_reject_small_fixtures() {
        let archive = archive_with_files(&[
            (PACKED_MANIFEST_ENTRY, b"name: demo\nversion: 0.1.0\n"),
            ("capsule.wasm", b"small wasm bytes"),
        ]);

        extract_manifest_yaml("demo", "0.1.0", &archive).unwrap();
        extract_root_wasm("demo", "0.1.0", &archive).unwrap();
    }
}
