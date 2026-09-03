use std::path::Path;

use thiserror::Error;
use zip::ZipArchive;

use crate::{
    build::PACKED_MANIFEST_ENTRY,
    manifest::{Manifest, ManifestError},
    registry::RuntimeType,
    zip_guard::{self, ZipGuardError},
};

/// Read the raw `murmur.yaml` text from a `.mur.zip` artifact file.
pub fn load_manifest_yaml_from_artifact(path: &Path) -> Result<String, ArtifactError> {
    let bytes = std::fs::read(path).map_err(|source| ArtifactError::Io {
        path: path.display().to_string(),
        source,
    })?;
    load_manifest_yaml_from_artifact_bytes(&bytes)
}

/// Read the raw `murmur.yaml` text from `.mur.zip` bytes.
pub fn load_manifest_yaml_from_artifact_bytes(bytes: &[u8]) -> Result<String, ArtifactError> {
    load_manifest_yaml_from_artifact_bytes_capped(
        bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn load_manifest_yaml_from_artifact_bytes_capped(
    bytes: &[u8],
    max_bytes: u64,
) -> Result<String, ArtifactError> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|source| ArtifactError::Zip {
        path: "<bytes>".to_string(),
        source,
    })?;

    match zip_guard::read_zip_entry_to_string_capped(&mut archive, PACKED_MANIFEST_ENTRY, max_bytes)
    {
        Ok(content) => Ok(content),
        Err(ZipGuardError::Zip(_)) => Err(ArtifactError::MissingManifest),
        Err(other) => Err(ArtifactError::ZipGuard(other)),
    }
}

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("failed to read artifact at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid artifact archive at {path}: {source}")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
    #[error("artifact missing required file murmur.yaml")]
    MissingManifest,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    ZipGuard(#[from] ZipGuardError),
}

pub fn load_manifest_from_artifact(path: &Path) -> Result<Manifest, ArtifactError> {
    let bytes = std::fs::read(path).map_err(|source| ArtifactError::Io {
        path: path.display().to_string(),
        source,
    })?;

    load_manifest_from_artifact_bytes(&bytes)
}

pub fn load_manifest_from_artifact_bytes(bytes: &[u8]) -> Result<Manifest, ArtifactError> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|source| ArtifactError::Zip {
        path: "<bytes>".to_string(),
        source,
    })?;

    let max_bytes = zip_guard::max_artifact_decompressed_bytes();
    let manifest_content = match zip_guard::read_zip_entry_to_string_capped(
        &mut archive,
        PACKED_MANIFEST_ENTRY,
        max_bytes,
    ) {
        Ok(content) => content,
        Err(ZipGuardError::Zip(_)) => return Err(ArtifactError::MissingManifest),
        Err(other) => return Err(ArtifactError::ZipGuard(other)),
    };

    Ok(Manifest::from_yaml_str(&manifest_content)?)
}

/// The packaging type and role an artifact's own packed `murmur.yaml` declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredRuntime {
    /// Packaging type — the value belonging in `ArtifactMeta::runtime`.
    pub runtime: RuntimeType,
    /// Role from `runtime:` — the value belonging in `ArtifactMeta::artifact_runtime`.
    pub artifact_runtime: String,
}

/// What `bytes` declare about themselves, for the fields of `ArtifactMeta` an artifact
/// determines rather than its installer.
///
/// `None` when `bytes` carry no readable `murmur.yaml`: not a zip, no manifest entry, or a
/// manifest that does not parse.
#[must_use]
pub fn declared_runtime_from_artifact_bytes(bytes: &[u8]) -> Option<DeclaredRuntime> {
    let manifest = load_manifest_from_artifact_bytes(bytes).ok()?;
    Some(DeclaredRuntime {
        runtime: manifest.registry_runtime(),
        artifact_runtime: manifest.runtime,
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
    fn load_manifest_yaml_reads_small_manifest() {
        let bytes = archive_with_files(&[("murmur.yaml", b"name: demo\nversion: 0.1.0\n")]);
        let content = load_manifest_yaml_from_artifact_bytes(&bytes).unwrap();
        assert!(content.contains("name: demo"));
    }

    #[test]
    fn load_manifest_yaml_missing_returns_missing_manifest_error() {
        let bytes = archive_with_files(&[("other.txt", b"hello")]);
        let err = load_manifest_yaml_from_artifact_bytes(&bytes).unwrap_err();
        assert!(matches!(err, ArtifactError::MissingManifest));
    }

    /// The archive-internal entry name this crate addressed before the manifest
    /// name was aligned across the zip boundary. No code path reads it; the two
    /// tests below exist precisely to prove that it is inert. This is the only
    /// place the retired name is spelled anywhere in `murmur-artifact`.
    const PRE_ALIGNMENT_ENTRY: &str = "manifest.yaml";

    #[test]
    fn packed_entry_is_addressed_as_murmur_yaml() {
        assert_eq!(PACKED_MANIFEST_ENTRY, "murmur.yaml");
    }

    #[test]
    fn load_manifest_yaml_ignores_pre_alignment_entry_name() {
        let bytes = archive_with_files(&[(PRE_ALIGNMENT_ENTRY, b"name: demo\nversion: 0.1.0\n")]);
        let err = load_manifest_yaml_from_artifact_bytes(&bytes).unwrap_err();
        assert!(
            matches!(err, ArtifactError::MissingManifest),
            "an archive carrying only the retired entry name must read as having no manifest at all; got: {err:?}"
        );
    }

    #[test]
    fn load_manifest_ignores_pre_alignment_entry_name() {
        let bytes = archive_with_files(&[(PRE_ALIGNMENT_ENTRY, b"name: demo\nversion: 0.1.0\n")]);
        let err = load_manifest_from_artifact_bytes(&bytes).unwrap_err();
        assert!(
            matches!(err, ArtifactError::MissingManifest),
            "an archive carrying only the retired entry name must read as having no manifest at all; got: {err:?}"
        );
    }

    #[test]
    fn load_manifest_reads_packed_entry() {
        let bytes = archive_with_files(&[(
            "murmur.yaml",
            b"name: demo\nversion: 0.1.0\nruntime: wasm\n",
        )]);
        let manifest = load_manifest_from_artifact_bytes(&bytes).unwrap();
        assert_eq!(manifest.name, "demo");
    }

    #[test]
    fn load_manifest_yaml_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let bytes = archive_with_files(&[("murmur.yaml", &big)]);

        let err = load_manifest_yaml_from_artifact_bytes_capped(&bytes, 16).unwrap_err();

        assert!(matches!(
            err,
            ArtifactError::ZipGuard(ZipGuardError::DecompressionCeilingExceeded { .. })
        ));
    }

    #[test]
    fn load_manifest_yaml_default_ceiling_does_not_reject_small_manifest() {
        let bytes = archive_with_files(&[("murmur.yaml", b"name: demo\nversion: 0.1.0\n")]);
        let content = load_manifest_yaml_from_artifact_bytes(&bytes).unwrap();
        assert!(content.contains("name: demo"));
    }
}
