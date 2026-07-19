use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub type Platform = (String, String);

pub const RESERVED_VERSIONS: [&str; 3] = ["latest", "stable", "edge"];

/// Registry package implementation type.
///
/// Describes the execution model for platform resolution and capability dispatch. In addition
/// to `wasm` (component bytes) and `native` (platform binary), `static` marks a payload with
/// no executable (e.g. a skill guidance file) that is installed for the agent to read directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeType {
    Wasm,
    Native,
    Static,
}

impl RuntimeType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::Native => "native",
            Self::Static => "static",
        }
    }
}

impl FromStr for RuntimeType {
    type Err = RegistryError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.to_ascii_lowercase().as_str() {
            "wasm" => Ok(Self::Wasm),
            "native" => Ok(Self::Native),
            "static" => Ok(Self::Static),
            _ => Err(RegistryError::InvalidInput(format!(
                "invalid runtime '{input}' (expected wasm|native|static)"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub name: String,
    pub version: String,
    pub runtime: RuntimeType,
    /// High-level role from the artifact manifest `runtime:` field (e.g. "driver", "tool",
    /// "hook", "skill"). Distinct from `runtime` (RuntimeType), which describes the execution
    /// model (wasm/native/skill) used for platform resolution and capability dispatch.
    pub artifact_runtime: String,
    #[serde(default)]
    pub platforms: Vec<Platform>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub artifact_id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtifact {
    pub meta: ArtifactMeta,
    pub bytes: Bytes,
    pub sha256: String,
}

pub trait Registry: Send + Sync {
    fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError>;

    /// Resolve an artifact, preferring a platform-specific variant when `platform` is `Some`.
    ///
    /// For WASM artifacts, implementations should ignore `platform` and behave like `resolve`.
    /// For native artifacts, implementations should prefer the platform-tagged file and fall back
    /// to the generic file when no tagged variant exists.
    fn resolve_with_platform(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        let _ = platform;
        self.resolve(name, version)
    }

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError>;
    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError>;
}

#[derive(Debug, Clone)]
pub struct LocalRegistry {
    root: PathBuf,
}

impl LocalRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn from_default_home() -> Result<Self, RegistryError> {
        Ok(Self::new(default_registry_root()?))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn artifact_path_for(&self, name: &str, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(format!("{name}-{version}.mur.zip"))
    }

    #[must_use]
    pub fn sha256_path_for(&self, name: &str, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(format!("{name}-{version}.sha256"))
    }

    /// Path for a platform-specific artifact variant (e.g. `git-tool-0.4.0-darwin-aarch64.mur.zip`).
    #[must_use]
    pub fn artifact_path_for_platform(&self, name: &str, version: &str, platform: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(format!("{name}-{version}-{platform}.mur.zip"))
    }

    #[must_use]
    fn sha256_path_for_platform(&self, name: &str, version: &str, platform: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(format!("{name}-{version}-{platform}.sha256"))
    }

    fn artifact_dir(&self, name: &str, version: &str) -> PathBuf {
        self.root.join(name).join(version)
    }

    fn metadata_path_for(&self, name: &str, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(format!("{name}-{version}.meta.json"))
    }

    // Currently at most one platform per artifact. Vec<Platform> is kept for forward
    // compatibility but only the first entry is used when writing/reading storage paths.
    fn platform_key(meta: &ArtifactMeta) -> Option<String> {
        meta.platforms
            .first()
            .map(|(os, arch)| format!("{os}-{arch}"))
    }

    pub fn store_installed_overwrite(
        &self,
        meta: ArtifactMeta,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<(), RegistryError> {
        verify_sha256(&meta.name, &meta.version, bytes, expected_sha256)?;
        self.write_artifact_set(&meta, bytes, expected_sha256, true)
    }

    fn write_artifact_set(
        &self,
        meta: &ArtifactMeta,
        bytes: &[u8],
        sha256: &str,
        overwrite: bool,
    ) -> Result<(), RegistryError> {
        let dir = self.artifact_dir(&meta.name, &meta.version);
        fs::create_dir_all(&dir).map_err(|source| RegistryError::Io {
            path: dir.display().to_string(),
            source,
        })?;

        let platform = Self::platform_key(meta);
        let artifact_path = if let Some(ref p) = platform {
            self.artifact_path_for_platform(&meta.name, &meta.version, p)
        } else {
            self.artifact_path_for(&meta.name, &meta.version)
        };
        let hash_path = if let Some(ref p) = platform {
            self.sha256_path_for_platform(&meta.name, &meta.version, p)
        } else {
            self.sha256_path_for(&meta.name, &meta.version)
        };
        let metadata_path = self.metadata_path_for(&meta.name, &meta.version);

        if !overwrite && (artifact_path.exists() || hash_path.exists()) {
            return Err(RegistryError::Conflict {
                name: meta.name.clone(),
                version: meta.version.clone(),
            });
        }

        let seed = unique_seed();
        let artifact_tmp = temp_path(&artifact_path, "zip", seed);
        let hash_tmp = temp_path(&hash_path, "sha", seed);
        let metadata_tmp = temp_path(&metadata_path, "meta", seed);

        let write_and_commit = (|| {
            write_file_sync(&artifact_tmp, bytes)?;
            write_file_sync(&hash_tmp, sha256.as_bytes())?;
            write_metadata(&metadata_tmp, &IndexMetadata { meta: meta.clone() }).map_err(
                |source| RegistryError::Io {
                    path: metadata_tmp.display().to_string(),
                    source,
                },
            )?;

            if overwrite {
                commit_overwrite(CommitPaths {
                    artifact_path: &artifact_path,
                    artifact_tmp: &artifact_tmp,
                    hash_path: &hash_path,
                    hash_tmp: &hash_tmp,
                    metadata_path: &metadata_path,
                    metadata_tmp: &metadata_tmp,
                })
            } else {
                commit_new(CommitPaths {
                    artifact_path: &artifact_path,
                    artifact_tmp: &artifact_tmp,
                    hash_path: &hash_path,
                    hash_tmp: &hash_tmp,
                    metadata_path: &metadata_path,
                    metadata_tmp: &metadata_tmp,
                })
            }
        })();

        if let Err(error) = write_and_commit {
            remove_file_if_exists(&artifact_tmp)?;
            remove_file_if_exists(&hash_tmp)?;
            remove_file_if_exists(&metadata_tmp)?;
            return Err(error);
        }

        Ok(())
    }

    fn resolve_impl(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        let metadata_path = self.metadata_path_for(name, version);

        // When a platform is requested, prefer the platform-specific file first, then
        // fall back to the generic (platform-agnostic) file so WASM artifacts always resolve.
        let (artifact_path, hash_path) = if let Some(p) = platform {
            let plat_artifact = self.artifact_path_for_platform(name, version, p);
            let plat_hash = self.sha256_path_for_platform(name, version, p);
            if plat_artifact.exists() && plat_hash.exists() {
                (plat_artifact, plat_hash)
            } else {
                (self.artifact_path_for(name, version), self.sha256_path_for(name, version))
            }
        } else {
            (self.artifact_path_for(name, version), self.sha256_path_for(name, version))
        };

        if !artifact_path.exists() || !hash_path.exists() {
            return Err(RegistryError::NotFound {
                name: name.to_string(),
                version: version.to_string(),
            });
        }

        let bytes = fs::read(&artifact_path).map_err(|source| RegistryError::Io {
            path: artifact_path.display().to_string(),
            source,
        })?;

        let sha256 = fs::read_to_string(&hash_path)
            .map_err(|source| RegistryError::Io {
                path: hash_path.display().to_string(),
                source,
            })?
            .trim()
            .to_string();

        if sha256.is_empty() {
            return Err(RegistryError::InvalidInput(format!(
                "artifact {name}@{version} has an empty sha256 sidecar"
            )));
        }

        let meta = if metadata_path.exists() {
            read_metadata(&metadata_path)
                .map_err(|source| RegistryError::Io {
                    path: metadata_path.display().to_string(),
                    source,
                })?
                .meta
        } else {
            fallback_meta(name, version)
        };

        Ok(ResolvedArtifact {
            meta,
            sha256,
            bytes: Bytes::from(bytes),
        })
    }
}

impl Registry for LocalRegistry {
    fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError> {
        self.resolve_impl(name, version, None)
    }

    fn resolve_with_platform(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        self.resolve_impl(name, version, platform)
    }

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError> {
        validate_version(&meta.version)?;

        let sha256 = sha256_hex(bytes);
        self.write_artifact_set(&meta, bytes, &sha256, false)?;

        Ok(PublishResult {
            artifact_id: format!("{}@{}", meta.name, meta.version),
            sha256,
        })
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }

        let mut index = Vec::new();

        for name_entry in fs::read_dir(&self.root).map_err(|source| RegistryError::Io {
            path: self.root.display().to_string(),
            source,
        })? {
            let name_entry = name_entry.map_err(|source| RegistryError::Io {
                path: self.root.display().to_string(),
                source,
            })?;
            let name_path = name_entry.path();
            if !name_path.is_dir() {
                continue;
            }

            let Some(name_name) = name_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };

            for version_entry in fs::read_dir(&name_path).map_err(|source| RegistryError::Io {
                path: name_path.display().to_string(),
                source,
            })? {
                let version_entry = version_entry.map_err(|source| RegistryError::Io {
                    path: name_path.display().to_string(),
                    source,
                })?;
                let version_path = version_entry.path();
                if !version_path.is_dir() {
                    continue;
                }

                let Some(version_name) = version_path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };

                let metadata_path = self.metadata_path_for(name_name, version_name);

                // Check for a platform-specific artifact OR the generic artifact.
                // A version directory may contain either `name-ver.mur.zip` (WASM/generic)
                // or `name-ver-<platform>.mur.zip` (native, platform-tagged).
                let has_generic = self.artifact_path_for(name_name, version_name).exists()
                    && self.sha256_path_for(name_name, version_name).exists();
                let has_any_platform = !has_generic && {
                    let prefix = format!("{name_name}-{version_name}-");
                    fs::read_dir(&version_path)
                        .ok()
                        .and_then(|entries| {
                            entries.flatten().any(|e| {
                                let fname = e.file_name();
                                let s = fname.to_string_lossy();
                                s.starts_with(&prefix) && s.ends_with(".mur.zip")
                            }).then_some(())
                        })
                        .is_some()
                };

                if !has_generic && !has_any_platform {
                    continue;
                }

                let mut meta = if metadata_path.exists() {
                    read_metadata(&metadata_path)
                        .map_err(|source| RegistryError::Io {
                            path: metadata_path.display().to_string(),
                            source,
                        })?
                        .meta
                } else {
                    fallback_meta(name_name, version_name)
                };

                meta.name = name_name.to_string();
                meta.version = version_name.to_string();
                index.push(meta);
            }
        }

        index.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.version.cmp(&b.version))
                .then_with(|| a.runtime.as_str().cmp(b.runtime.as_str()))
        });
        Ok(index)
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("artifact {name}@{version} not found")]
    NotFound { name: String, version: String },
    #[error("artifact {name}@{version} already exists")]
    Conflict { name: String, version: String },
    #[error("reserved artifact version '{0}' is not allowed")]
    ReservedVersion(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("artifact integrity mismatch for {name}@{version}: expected {expected}, got {actual}")]
    IntegrityMismatch {
        name: String,
        version: String,
        expected: String,
        actual: String,
    },
    #[error("could not determine home directory")]
    HomeDirNotFound,
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexMetadata {
    meta: ArtifactMeta,
}

struct CommitPaths<'a> {
    artifact_path: &'a Path,
    artifact_tmp: &'a Path,
    hash_path: &'a Path,
    hash_tmp: &'a Path,
    metadata_path: &'a Path,
    metadata_tmp: &'a Path,
}

fn commit_new(paths: CommitPaths<'_>) -> Result<(), RegistryError> {
    fs::rename(paths.artifact_tmp, paths.artifact_path).map_err(|source| RegistryError::Io {
        path: format!(
            "{} -> {}",
            paths.artifact_tmp.display(),
            paths.artifact_path.display()
        ),
        source,
    })?;

    if let Err(source) = fs::rename(paths.hash_tmp, paths.hash_path) {
        remove_file_if_exists(paths.artifact_path)?;
        return Err(RegistryError::Io {
            path: format!(
                "{} -> {}",
                paths.hash_tmp.display(),
                paths.hash_path.display()
            ),
            source,
        });
    }

    if let Err(source) = fs::rename(paths.metadata_tmp, paths.metadata_path) {
        remove_file_if_exists(paths.artifact_path)?;
        remove_file_if_exists(paths.hash_path)?;
        return Err(RegistryError::Io {
            path: format!(
                "{} -> {}",
                paths.metadata_tmp.display(),
                paths.metadata_path.display()
            ),
            source,
        });
    }

    Ok(())
}

fn commit_overwrite(paths: CommitPaths<'_>) -> Result<(), RegistryError> {
    let backup_seed = unique_seed();
    let artifact_backup = backup_path(paths.artifact_path, backup_seed);
    let hash_backup = backup_path(paths.hash_path, backup_seed);
    let metadata_backup = backup_path(paths.metadata_path, backup_seed);

    let had_artifact = move_existing(paths.artifact_path, &artifact_backup)?;
    let had_hash = move_existing(paths.hash_path, &hash_backup)?;
    let had_metadata = move_existing(paths.metadata_path, &metadata_backup)?;

    let mut committed_artifact = false;
    let mut committed_hash = false;
    let mut committed_metadata = false;

    let commit_result: Result<(), RegistryError> = (|| {
        fs::rename(paths.artifact_tmp, paths.artifact_path).map_err(|source| {
            RegistryError::Io {
                path: format!(
                    "{} -> {}",
                    paths.artifact_tmp.display(),
                    paths.artifact_path.display()
                ),
                source,
            }
        })?;
        committed_artifact = true;

        fs::rename(paths.hash_tmp, paths.hash_path).map_err(|source| RegistryError::Io {
            path: format!(
                "{} -> {}",
                paths.hash_tmp.display(),
                paths.hash_path.display()
            ),
            source,
        })?;
        committed_hash = true;

        fs::rename(paths.metadata_tmp, paths.metadata_path).map_err(|source| {
            RegistryError::Io {
                path: format!(
                    "{} -> {}",
                    paths.metadata_tmp.display(),
                    paths.metadata_path.display()
                ),
                source,
            }
        })?;
        committed_metadata = true;

        Ok(())
    })();

    if let Err(error) = commit_result {
        if committed_metadata {
            remove_file_if_exists(paths.metadata_path)?;
        }
        if committed_hash {
            remove_file_if_exists(paths.hash_path)?;
        }
        if committed_artifact {
            remove_file_if_exists(paths.artifact_path)?;
        }

        restore_backup(&metadata_backup, paths.metadata_path, had_metadata)?;
        restore_backup(&hash_backup, paths.hash_path, had_hash)?;
        restore_backup(&artifact_backup, paths.artifact_path, had_artifact)?;

        return Err(error);
    }

    remove_file_if_exists(&artifact_backup)?;
    remove_file_if_exists(&hash_backup)?;
    remove_file_if_exists(&metadata_backup)?;

    Ok(())
}

fn move_existing(from: &Path, to: &Path) -> Result<bool, RegistryError> {
    if !from.exists() {
        return Ok(false);
    }

    fs::rename(from, to).map_err(|source| RegistryError::Io {
        path: format!("{} -> {}", from.display(), to.display()),
        source,
    })?;
    Ok(true)
}

fn restore_backup(backup: &Path, final_path: &Path, had_backup: bool) -> Result<(), RegistryError> {
    if !had_backup {
        return Ok(());
    }

    fs::rename(backup, final_path).map_err(|source| RegistryError::Io {
        path: format!("{} -> {}", backup.display(), final_path.display()),
        source,
    })
}

fn fallback_meta(name: &str, version: &str) -> ArtifactMeta {
    ArtifactMeta {
        name: name.to_string(),
        version: version.to_string(),
        runtime: RuntimeType::Wasm,
        artifact_runtime: String::new(),
        platforms: Vec::new(),
        description: None,
        tags: Vec::new(),
    }
}

fn read_metadata(path: &Path) -> Result<IndexMetadata, io::Error> {
    let raw = fs::read(path)?;
    serde_json::from_slice(&raw).map_err(invalid_data)
}

fn write_metadata(path: &Path, metadata: &IndexMetadata) -> Result<(), io::Error> {
    let json = serde_json::to_vec_pretty(metadata).map_err(invalid_data)?;
    write_file_sync_io(path, &json)
}

fn write_file_sync(path: &Path, bytes: &[u8]) -> Result<(), RegistryError> {
    write_file_sync_io(path, bytes).map_err(|source| RegistryError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn write_file_sync_io(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), RegistryError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RegistryError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn temp_path(final_path: &Path, kind: &str, seed: u128) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact");
    final_path.with_file_name(format!(".{name}.{kind}.{seed}.tmp"))
}

fn backup_path(final_path: &Path, seed: u128) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact");
    final_path.with_file_name(format!(".{name}.{seed}.bak"))
}

fn unique_seed() -> u128 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    nanos ^ u128::from(std::process::id())
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub fn validate_version(version: &str) -> Result<(), RegistryError> {
    if is_reserved_version(version) {
        return Err(RegistryError::ReservedVersion(version.to_string()));
    }

    Ok(())
}

#[must_use]
pub fn is_reserved_version(version: &str) -> bool {
    RESERVED_VERSIONS
        .iter()
        .any(|reserved| version.eq_ignore_ascii_case(reserved))
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn verify_sha256(
    name: &str,
    version: &str,
    bytes: &[u8],
    expected_sha256: &str,
) -> Result<(), RegistryError> {
    let actual = sha256_hex(bytes);
    if actual != expected_sha256 {
        return Err(RegistryError::IntegrityMismatch {
            name: name.to_string(),
            version: version.to_string(),
            expected: expected_sha256.to_string(),
            actual,
        });
    }

    Ok(())
}

fn default_registry_root() -> Result<PathBuf, RegistryError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(RegistryError::HomeDirNotFound)?;

    let mut root = PathBuf::from(home);
    if !root.is_absolute() {
        root = env::current_dir()
            .map_err(|source| RegistryError::Io {
                path: "current_dir".to_string(),
                source,
            })?
            .join(root);
    }

    Ok(root.join(".murmur").join("artifacts"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_meta(name: &str, version: &str) -> ArtifactMeta {
        ArtifactMeta {
            name: name.to_string(),
            version: version.to_string(),
            runtime: RuntimeType::Wasm,
            artifact_runtime: "wasm".to_string(),
            platforms: Vec::new(),
            description: Some("demo".to_string()),
            tags: vec!["test".to_string()],
        }
    }

    #[test]
    fn publish_writes_expected_artifact_and_sha_sidecar() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = test_meta("hello", "0.0.2");
        let bytes = b"artifact-contents";

        let published = registry.publish(meta.clone(), bytes).unwrap();
        assert_eq!(published.artifact_id, "hello@0.0.2");

        let artifact_path = dir.path().join("hello/0.0.2/hello-0.0.2.mur.zip");
        let sha_path = dir.path().join("hello/0.0.2/hello-0.0.2.sha256");

        assert!(artifact_path.exists());
        assert!(sha_path.exists());
        assert_eq!(fs::read(&artifact_path).unwrap(), bytes);
        assert_eq!(
            fs::read_to_string(&sha_path).unwrap().trim(),
            sha256_hex(bytes)
        );
    }

    #[test]
    fn resolve_returns_original_bytes_and_sidecar_hash() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = b"abc123";
        registry
            .publish(test_meta("hello", "0.0.2"), bytes)
            .unwrap();

        let resolved = registry.resolve("hello", "0.0.2").unwrap();
        assert_eq!(resolved.bytes, Bytes::from_static(bytes));
        assert_eq!(resolved.meta.name, "hello");
        assert_eq!(resolved.sha256, sha256_hex(bytes));
    }

    #[test]
    fn resolve_missing_returns_not_found() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());

        let err = registry.resolve("missing", "1.0.0").unwrap_err();
        assert!(matches!(
            err,
            RegistryError::NotFound { name, version } if name == "missing" && version == "1.0.0"
        ));
    }

    #[test]
    fn reserved_versions_are_rejected() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());

        for version in RESERVED_VERSIONS {
            let err = registry
                .publish(test_meta("hello", version), b"x")
                .unwrap_err();
            assert!(matches!(
                err,
                RegistryError::ReservedVersion(v) if v.eq_ignore_ascii_case(version)
            ));
        }
    }

    #[test]
    fn hash_helpers_match_and_detect_tampering() {
        let bytes = b"payload";
        let expected = sha256_hex(bytes);
        verify_sha256("hello", "0.0.2", bytes, &expected).unwrap();

        let err = verify_sha256("hello", "0.0.2", b"tampered", &expected).unwrap_err();
        assert!(matches!(
            err,
            RegistryError::IntegrityMismatch { name, version, .. } if name == "hello" && version == "0.0.2"
        ));
    }

    #[test]
    fn duplicate_publish_returns_conflict() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry
            .publish(test_meta("hello", "0.0.2"), b"v1")
            .unwrap();

        let err = registry
            .publish(test_meta("hello", "0.0.2"), b"v2")
            .unwrap_err();
        assert!(matches!(
            err,
            RegistryError::Conflict { name, version } if name == "hello" && version == "0.0.2"
        ));
    }

    #[test]
    fn install_overwrite_replaces_existing_zip_and_sha() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = test_meta("hello", "0.0.2");
        registry.publish(meta.clone(), b"old-bytes").unwrap();

        let new_bytes = b"new-bytes";
        let new_sha = sha256_hex(new_bytes);
        registry
            .store_installed_overwrite(meta, new_bytes, &new_sha)
            .unwrap();

        let resolved = registry.resolve("hello", "0.0.2").unwrap();
        assert_eq!(resolved.bytes, Bytes::from_static(new_bytes));
        assert_eq!(resolved.sha256, new_sha);
    }

    #[test]
    fn publish_failure_leaves_no_partial_files() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = test_meta("hello", "0.0.2");

        let target_dir = dir.path().join("hello/0.0.2");
        fs::create_dir_all(&target_dir).unwrap();

        let original_perms = fs::metadata(&target_dir).unwrap().permissions();
        let mut read_only_perms = original_perms.clone();
        read_only_perms.set_readonly(true);
        fs::set_permissions(&target_dir, read_only_perms).unwrap();

        let _ = registry.publish(meta, b"bytes").unwrap_err();

        let artifact_path = dir.path().join("hello/0.0.2/hello-0.0.2.mur.zip");
        let sha_path = dir.path().join("hello/0.0.2/hello-0.0.2.sha256");
        assert!(!artifact_path.exists());
        assert!(!sha_path.exists());

        fs::set_permissions(&target_dir, original_perms).unwrap();
    }

    #[test]
    fn list_index_reads_metadata_sidecars() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry.publish(test_meta("a", "1.0.0"), b"a").unwrap();
        registry.publish(test_meta("b", "2.0.0"), b"b").unwrap();

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 2);
        assert_eq!(index[0].name, "a");
        assert_eq!(index[1].name, "b");
    }

    #[test]
    fn static_runtime_type_as_str_returns_static() {
        assert_eq!(RuntimeType::Static.as_str(), "static");
    }

    #[test]
    fn static_runtime_type_roundtrips_via_serde() {
        let json = serde_json::to_string(&RuntimeType::Static).unwrap();
        assert_eq!(json, "\"static\"");
        let back: RuntimeType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RuntimeType::Static);
    }

    #[test]
    fn static_artifact_publishes_with_static_runtime_type() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = ArtifactMeta {
            name: "my-skill".to_string(),
            version: "0.1.0".to_string(),
            runtime: RuntimeType::Static,
            artifact_runtime: "skill".to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
        };
        registry.publish(meta, b"skill-zip-bytes").unwrap();

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].runtime, RuntimeType::Static);
        assert_eq!(index[0].runtime.as_str(), "static");
    }
}
