//! Shared hardening primitives for reading `.mur.zip` archives.
//!
//! Every place that opens a `.mur.zip` and reads an entry should validate the entry's name
//! with [`sanitize_entry_path`] (or [`resolve_within`], when the bytes will be written to a
//! target directory) and read its decompressed bytes with [`read_zip_entry_capped`] or
//! [`read_zip_entry_to_string_capped`] rather than calling `zip::ZipArchive` directly. This
//! keeps every `.mur.zip` reader safe against path traversal and decompression-bomb entries
//! without duplicating the checks at each call site.

use std::{
    env,
    io::{Read, Seek},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use zip::ZipArchive;

/// Default decompressed-bytes ceiling applied to a single zip entry read, when
/// [`MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV`] is unset or unparseable.
pub const DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES: u64 = 500 * 1024 * 1024;

/// Environment variable overriding [`DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES`].
pub const MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV: &str = "MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES";

/// The decompressed-bytes ceiling to apply to a single zip entry read: the value of
/// [`MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV`] if set and a valid `u64`, else
/// [`DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES`].
#[must_use]
pub fn max_artifact_decompressed_bytes() -> u64 {
    env::var(MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES)
}

#[derive(Debug, Error)]
pub enum ZipGuardError {
    #[error("zip entry '{name}' has an unsafe path: {reason}")]
    UnsafeEntryPath { name: String, reason: String },
    #[error("zip entry '{name}' resolves outside the target directory '{base}'")]
    PathEscapesTarget { name: String, base: String },
    #[error("zip entry '{name}' exceeds the {limit}-byte decompression ceiling")]
    DecompressionCeilingExceeded { name: String, limit: u64 },
    #[error("zip entry '{name}' is not valid UTF-8")]
    InvalidUtf8 { name: String },
    #[error("zip archive error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("io error reading zip entry: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate a raw zip entry name and return a safe, relative path.
///
/// Strips a leading `/`, then walks the remaining path components and rejects any entry
/// containing a `..` component (or, on platforms where it's meaningful, a drive prefix).
/// Does not touch the filesystem or check anything against a target directory — see
/// [`resolve_within`] for that.
pub fn sanitize_entry_path(raw_name: &str) -> Result<PathBuf, ZipGuardError> {
    let stripped = raw_name.trim_start_matches('/');
    let path = Path::new(stripped);

    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(ZipGuardError::UnsafeEntryPath {
                    name: raw_name.to_string(),
                    reason: "contains a '..' component".to_string(),
                });
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(ZipGuardError::UnsafeEntryPath {
                    name: raw_name.to_string(),
                    reason: "contains an absolute path component".to_string(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    Ok(path.to_path_buf())
}

/// Sanitize `raw_name` (see [`sanitize_entry_path`]) and join it to `base`, verifying that
/// the resolved path is still contained within `base`.
///
/// Intended for call sites that extract a zip entry to a path on disk under a known target
/// directory. Entry names are already rejected by [`sanitize_entry_path`] if they contain a
/// `..` component, so the containment check here is defense-in-depth rather than the primary
/// guard.
pub fn resolve_within(base: &Path, raw_name: &str) -> Result<PathBuf, ZipGuardError> {
    let relative = sanitize_entry_path(raw_name)?;
    let joined = base.join(&relative);

    if !joined.starts_with(base) {
        return Err(ZipGuardError::PathEscapesTarget {
            name: raw_name.to_string(),
            base: base.display().to_string(),
        });
    }

    Ok(joined)
}

/// Read a zip entry's full decompressed bytes, failing once more than `max_bytes` have been
/// produced rather than materializing the full (potentially huge) entry first.
pub fn read_zip_entry_capped<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    entry_name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ZipGuardError> {
    let file = archive.by_name(entry_name)?;
    read_capped(file, entry_name, max_bytes)
}

/// Like [`read_zip_entry_capped`], but decodes the capped bytes as UTF-8 text.
pub fn read_zip_entry_to_string_capped<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    entry_name: &str,
    max_bytes: u64,
) -> Result<String, ZipGuardError> {
    let bytes = read_zip_entry_capped(archive, entry_name, max_bytes)?;
    String::from_utf8(bytes).map_err(|_| ZipGuardError::InvalidUtf8 {
        name: entry_name.to_string(),
    })
}

fn read_capped<R: Read>(
    mut reader: R,
    entry_name: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, ZipGuardError> {
    let mut buf = Vec::new();
    let mut limited = (&mut reader).take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut buf)?;

    if buf.len() as u64 > max_bytes {
        return Err(ZipGuardError::DecompressionCeilingExceeded {
            name: entry_name.to_string(),
            limit: max_bytes,
        });
    }

    Ok(buf)
}

/// Serializes tests (in this module and elsewhere in the crate) that mutate
/// [`MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV`] — the env var is process-global, so concurrent
/// mutation from parallel test threads would be flaky without this lock.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    fn sanitize_entry_path_strips_leading_slash() {
        let path = sanitize_entry_path("/capsule.wasm").unwrap();
        assert_eq!(path, Path::new("capsule.wasm"));
    }

    #[test]
    fn sanitize_entry_path_rejects_parent_dir_component() {
        let err = sanitize_entry_path("../../evil.wasm").unwrap_err();
        assert!(matches!(err, ZipGuardError::UnsafeEntryPath { .. }));
    }

    #[test]
    fn sanitize_entry_path_rejects_embedded_parent_dir_component() {
        let err = sanitize_entry_path("sub/../../evil.wasm").unwrap_err();
        assert!(matches!(err, ZipGuardError::UnsafeEntryPath { .. }));
    }

    #[test]
    fn sanitize_entry_path_allows_nested_normal_path() {
        let path = sanitize_entry_path("tools/echo/murmur.yaml").unwrap();
        assert_eq!(path, Path::new("tools/echo/murmur.yaml"));
    }

    #[test]
    fn resolve_within_stays_inside_base() {
        let base = Path::new("/tmp/workdir/tools/echo");
        let resolved = resolve_within(base, "murmur.yaml").unwrap();
        assert_eq!(resolved, base.join("murmur.yaml"));
    }

    #[test]
    fn resolve_within_rejects_traversal() {
        let base = Path::new("/tmp/workdir/tools/echo");
        let err = resolve_within(base, "../../../etc/passwd").unwrap_err();
        assert!(matches!(err, ZipGuardError::UnsafeEntryPath { .. }));
    }

    #[test]
    fn read_zip_entry_capped_reads_small_entry() {
        let bytes = archive_with_files(&[("murmur.yaml", b"name: demo\n")]);
        let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();

        let content = read_zip_entry_capped(&mut archive, "murmur.yaml", 1024).unwrap();
        assert_eq!(content, b"name: demo\n");
    }

    #[test]
    fn read_zip_entry_capped_rejects_oversized_entry() {
        let big = vec![b'a'; 2048];
        let bytes = archive_with_files(&[("murmur.yaml", &big)]);
        let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();

        let err = read_zip_entry_capped(&mut archive, "murmur.yaml", 1024).unwrap_err();
        assert!(matches!(
            err,
            ZipGuardError::DecompressionCeilingExceeded { limit: 1024, .. }
        ));
    }

    #[test]
    fn read_zip_entry_to_string_capped_reads_text() {
        let bytes = archive_with_files(&[("murmur.yaml", b"name: demo\nversion: 0.1.0\n")]);
        let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();

        let content =
            read_zip_entry_to_string_capped(&mut archive, "murmur.yaml", 1024).unwrap();
        assert!(content.contains("name: demo"));
    }

    #[test]
    fn max_artifact_decompressed_bytes_defaults_without_env() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        env::remove_var(MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV);
        assert_eq!(
            max_artifact_decompressed_bytes(),
            DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES
        );
    }

    #[test]
    fn max_artifact_decompressed_bytes_honors_env_override() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        env::set_var(MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV, "12345");
        let result = max_artifact_decompressed_bytes();
        env::remove_var(MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV);
        assert_eq!(result, 12345);
    }
}
