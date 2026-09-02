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

use crate::{
    artifact::declared_runtime_from_artifact_bytes,
    platform::split_platform_tag,
    wit_contract::{wit_contracts_from_artifact_bytes, WitContracts},
};

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
    /// The versioned WIT interfaces the packed component declares, derived from the artifact
    /// bytes by [`LocalRegistry`] on every write. Absent when the artifact carries no readable
    /// component — a native binary, a skill, a core module, or a payload that does not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wit_contracts: Option<WitContracts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub artifact_id: String,
    pub sha256: String,
}

/// How the payload a resolve returned relates to the platform that was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMatch {
    /// No platform was requested, or the payload needs none (WASM, static).
    NotApplicable,
    /// A platform-tagged payload for the requested platform.
    Tagged,
    /// A native payload returned from the generic, untagged path because no tagged
    /// payload exists — resolvable, but nothing recorded which platform it is for.
    UntaggedFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedArtifact {
    pub meta: ArtifactMeta,
    pub bytes: Bytes,
    pub sha256: String,
    /// Which store path these bytes came off, relative to the platform that was asked for.
    /// [`PlatformMatch::UntaggedFallback`] is what `W-REG-001` reports.
    pub platform_match: PlatformMatch,
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

    /// Path of the metadata sidecar describing one payload.
    ///
    /// `Some(platform)` names the sidecar beside the platform-tagged payload,
    /// `None` the one beside the generic payload. One version directory holds one sidecar per
    /// payload in it: a single untagged file would be overwritten by every platform installed
    /// after the first, leaving the store claiming one platform's provenance for all of them.
    #[must_use]
    pub fn metadata_path_for(&self, name: &str, version: &str, platform: Option<&str>) -> PathBuf {
        let file_name = match platform {
            Some(platform) => format!("{name}-{version}-{platform}.meta.json"),
            None => format!("{name}-{version}.meta.json"),
        };
        self.artifact_dir(name, version).join(file_name)
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
        let metadata_path = self.metadata_path_for(&meta.name, &meta.version, platform.as_deref());

        if !overwrite && (artifact_path.exists() || hash_path.exists()) {
            return Err(RegistryError::Conflict {
                name: meta.name.clone(),
                version: meta.version.clone(),
            });
        }

        // The contracts describe these exact bytes, so they are read from them rather than
        // taken from the caller: a hand-authored value cannot survive a store and so cannot
        // drift from the artifact it describes.
        let mut meta = meta.clone();
        meta.wit_contracts = wit_contracts_from_artifact_bytes(bytes);

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
        // When a platform is requested, prefer the platform-specific file first, then
        // fall back to the generic (platform-agnostic) file so WASM artifacts always resolve.
        // `path_platform` is the tag of the payload actually chosen, and every sidecar below is
        // read from beside that payload rather than from a single file per name+version.
        let (artifact_path, hash_path, path_platform) = if let Some(p) = platform {
            let plat_artifact = self.artifact_path_for_platform(name, version, p);
            let plat_hash = self.sha256_path_for_platform(name, version, p);
            if plat_artifact.exists() && plat_hash.exists() {
                (plat_artifact, plat_hash, Some(p))
            } else {
                (
                    self.artifact_path_for(name, version),
                    self.sha256_path_for(name, version),
                    None,
                )
            }
        } else {
            (
                self.artifact_path_for(name, version),
                self.sha256_path_for(name, version),
                None,
            )
        };
        let metadata_path = self.metadata_path_for(name, version, path_platform);

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

        let mut meta = if metadata_path.exists() {
            read_metadata(&metadata_path)
                .map_err(|source| RegistryError::Io {
                    path: metadata_path.display().to_string(),
                    source,
                })?
                .meta
        } else {
            derived_meta(name, version, &bytes, path_platform)
        };

        // What an artifact is, is what its own packed murmur.yaml says: a payload readable here
        // overrules the sidecar rather than the other way round, so a value recorded wrongly by
        // an earlier install cannot outlive the next resolve. The recorded value stands only for
        // bytes carrying no readable manifest, which say nothing to prefer over it.
        if let Some(declared) = declared_runtime_from_artifact_bytes(&bytes) {
            meta.runtime = declared.runtime;
            meta.artifact_runtime = declared.artifact_runtime;
        }

        // A native payload off the generic path resolves, and runs — but nothing on disk says
        // which platform it was built for, so a second platform installed into this version
        // directory would overwrite it. `W-REG-001` reports that; a WASM payload off the same
        // path is simply how a platform-independent artifact is stored and is not reported.
        let platform_match = match (platform, path_platform) {
            (Some(_), Some(_)) => PlatformMatch::Tagged,
            (Some(_), None) if meta.runtime == RuntimeType::Native => {
                PlatformMatch::UntaggedFallback
            }
            _ => PlatformMatch::NotApplicable,
        };

        Ok(ResolvedArtifact {
            meta,
            sha256,
            bytes: Bytes::from(bytes),
            platform_match,
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

                // A version directory may contain the generic `name-ver.mur.zip` (WASM,
                // static) or one `name-ver-<platform>.mur.zip` per platform (native), and one
                // metadata sidecar beside each payload.
                let mut payloads: Vec<PathBuf> = Vec::new();
                let mut sidecars: Vec<PathBuf> = Vec::new();
                let prefix = format!("{name_name}-{version_name}");
                let mut names: Vec<String> = fs::read_dir(&version_path)
                    .map_err(|source| RegistryError::Io {
                        path: version_path.display().to_string(),
                        source,
                    })?
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort_unstable();
                for file_name in names {
                    let Some(tail) = file_name.strip_prefix(&prefix) else {
                        continue;
                    };
                    if !tail.is_empty() && !tail.starts_with('-') && !tail.starts_with('.') {
                        continue;
                    }
                    if file_name.ends_with(".mur.zip") {
                        payloads.push(version_path.join(&file_name));
                    } else if file_name.ends_with(".meta.json") {
                        sidecars.push(version_path.join(&file_name));
                    }
                }

                let has_generic = self.artifact_path_for(name_name, version_name).exists()
                    && self.sha256_path_for(name_name, version_name).exists();
                if !has_generic && payloads.is_empty() {
                    continue;
                }

                // One row per name+version, however many payloads back it: `mur list` reports
                // an artifact, and `platforms` is the set of platforms that artifact is
                // installed for. A row per sidecar would list the same artifact twice.
                let mut sidecar_metas = Vec::with_capacity(sidecars.len());
                for sidecar in &sidecars {
                    sidecar_metas.push(
                        read_metadata(sidecar)
                            .map_err(|source| RegistryError::Io {
                                path: sidecar.display().to_string(),
                                source,
                            })?
                            .meta,
                    );
                }

                let mut meta = match sidecar_metas.first() {
                    Some(first) => {
                        let mut meta = first.clone();
                        meta.platforms = sidecar_metas
                            .iter()
                            .flat_map(|meta| meta.platforms.iter().cloned())
                            .collect();
                        meta.platforms.sort_unstable();
                        meta.platforms.dedup();
                        meta
                    }
                    // No sidecar anywhere in this directory: read the payload rather than
                    // guessing, the same derivation `resolve_impl` makes. Only a directory
                    // missing its sidecars pays for this read.
                    None => {
                        let payload = if has_generic {
                            self.artifact_path_for(name_name, version_name)
                        } else {
                            payloads[0].clone()
                        };
                        let path_platform = payload
                            .file_name()
                            .and_then(|name| name.to_str())
                            .and_then(crate::platform::split_platform_suffix)
                            .map(|(_, platform)| platform);
                        let bytes = fs::read(&payload).map_err(|source| RegistryError::Io {
                            path: payload.display().to_string(),
                            source,
                        })?;
                        derived_meta(name_name, version_name, &bytes, path_platform)
                    }
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

/// Describe a payload that has no metadata sidecar, from the payload itself.
///
/// A store written before sidecars were per-platform, or seeded by a path that wrote only the
/// zip and its hash, still has to answer what the artifact is. `runtime` and `artifact_runtime`
/// come from the `murmur.yaml` packed inside the bytes and `platforms` from the tag of the path
/// the bytes were found at, so the answer describes these bytes rather than a default. A payload
/// that does not parse falls back to `wasm` with nothing else filled in, so an opaque payload
/// still resolves.
fn derived_meta(
    name: &str,
    version: &str,
    bytes: &[u8],
    path_platform: Option<&str>,
) -> ArtifactMeta {
    let declared = declared_runtime_from_artifact_bytes(bytes);
    ArtifactMeta {
        name: name.to_string(),
        version: version.to_string(),
        runtime: declared
            .as_ref()
            .map_or(RuntimeType::Wasm, |declared| declared.runtime),
        artifact_runtime: declared
            .as_ref()
            .map_or_else(String::new, |declared| declared.artifact_runtime.clone()),
        platforms: path_platform
            .and_then(split_platform_tag)
            .map(|(os, arch)| vec![(os.to_string(), arch.to_string())])
            .unwrap_or_default(),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
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

/// SHA-256 of everything `reader` yields, with the byte count hashed alongside it.
///
/// The streaming companion to [`sha256_hex`], sharing its one `Sha256`: a caller that must hash a
/// file it cannot afford to hold in memory — a resource-plane listing walking a subtree it does
/// not bound — gets the same digest without a second hasher entering the workspace.
pub fn sha256_hex_of_reader(reader: &mut impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total = total.saturating_add(read as u64);
    }
    Ok((total, format!("{:x}", hasher.finalize())))
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
            wit_contracts: None,
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
            wit_contracts: None,
        };
        registry.publish(meta, b"skill-zip-bytes").unwrap();

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].runtime, RuntimeType::Static);
        assert_eq!(index[0].runtime.as_str(), "static");
    }

    // ── wit_contracts ─────────────────────────────────────────────────────────

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Cursor;
        use zip::{write::SimpleFileOptions, ZipWriter};

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

    fn tool_component() -> Vec<u8> {
        wat::parse_str(
            r#"
            (component
              (import "murmur:tool-registry/invoke@0.1.0" (instance))
              (core module $m (func (export "run")))
              (core instance $i (instantiate $m))
              (func $run (canon lift (core func $i "run")))
              (instance $iface (export "run" (func $run)))
              (export "murmur:tool/run@0.1.0" (instance $iface))
            )
            "#,
        )
        .unwrap()
    }

    #[test]
    fn publish_records_the_contracts_the_component_declares() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[("tool.wasm", &tool_component())]);

        registry.publish(test_meta("ct", "0.1.0"), &bytes).unwrap();

        let contracts = registry
            .resolve("ct", "0.1.0")
            .unwrap()
            .meta
            .wit_contracts
            .unwrap();
        assert_eq!(contracts.exports, vec!["murmur:tool/run@0.1.0"]);
        assert_eq!(contracts.imports, vec!["murmur:tool-registry/invoke@0.1.0"]);
    }

    #[test]
    fn publish_replaces_a_caller_supplied_value_with_the_extracted_one() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let mut meta = test_meta("ct", "0.1.0");
        meta.wit_contracts = Some(WitContracts {
            exports: vec!["murmur:hook/lifecycle@9.9.9".to_string()],
            imports: vec!["murmur:nothing/at-all@9.9.9".to_string()],
        });

        registry
            .publish(meta, &zip_with(&[("tool.wasm", &tool_component())]))
            .unwrap();

        let contracts = registry
            .resolve("ct", "0.1.0")
            .unwrap()
            .meta
            .wit_contracts
            .unwrap();
        assert_eq!(contracts.exports, vec!["murmur:tool/run@0.1.0"]);
        assert_eq!(contracts.imports, vec!["murmur:tool-registry/invoke@0.1.0"]);
    }

    #[test]
    fn a_fabricated_value_does_not_survive_a_payload_with_no_component() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let mut meta = test_meta("skill", "0.1.0");
        meta.wit_contracts = Some(WitContracts {
            exports: vec!["murmur:hook/lifecycle@9.9.9".to_string()],
            imports: Vec::new(),
        });

        registry
            .publish(meta, &zip_with(&[("skill.md", b"# guidance")]))
            .unwrap();

        assert!(registry
            .resolve("skill", "0.1.0")
            .unwrap()
            .meta
            .wit_contracts
            .is_none());
    }

    #[test]
    fn payloads_with_nothing_to_extract_omit_the_key_entirely() {
        let module = wat::parse_str(r#"(module (func (export "f")))"#).unwrap();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("skill", zip_with(&[("skill.md", b"# guidance")])),
            ("native", zip_with(&[("bin/native", b"\x7fELF")])),
            ("stub", zip_with(&[("tool.wasm", b"\0asm")])),
            ("module", zip_with(&[("tool.wasm", &module)])),
            (
                "ambiguous",
                zip_with(&[("a.wasm", b"\0asm"), ("b.wasm", b"\0asm")]),
            ),
            ("not-a-zip", b"fake-artifact-bytes".to_vec()),
        ];

        for (name, bytes) in cases {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());
            registry.publish(test_meta(name, "0.1.0"), &bytes).unwrap();

            let raw = fs::read_to_string(registry.metadata_path_for(name, "0.1.0", None)).unwrap();
            assert!(
                !raw.contains("wit_contracts"),
                "{name}: metadata carries a wit_contracts key: {raw}"
            );
            assert!(registry
                .resolve(name, "0.1.0")
                .unwrap()
                .meta
                .wit_contracts
                .is_none());
        }
    }

    // ── platform provenance ───────────────────────────────────────────────────

    fn native_meta(name: &str, version: &str, platform: &str) -> ArtifactMeta {
        let (os, arch) = crate::platform::split_platform_tag(platform).unwrap();
        ArtifactMeta {
            name: name.to_string(),
            version: version.to_string(),
            runtime: RuntimeType::Native,
            artifact_runtime: "tool".to_string(),
            platforms: vec![(os.to_string(), arch.to_string())],
            description: None,
            tags: Vec::new(),
            wit_contracts: None,
        }
    }

    /// A `.mur.zip` whose packed manifest declares a native tool, with `filler` bytes in the
    /// binary so two platforms' payloads hash differently.
    fn native_artifact_zip(name: &str, version: &str, filler: &[u8]) -> Vec<u8> {
        let manifest =
            format!("name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n");
        zip_with(&[
            ("murmur.yaml", manifest.as_bytes()),
            (&format!("bin/{name}"), filler),
        ])
    }

    #[test]
    fn two_platforms_install_into_one_version_directory_without_clobbering() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let linux = native_artifact_zip("nativetool", "0.1.0", b"linux-binary");
        let darwin = native_artifact_zip("nativetool", "0.1.0", b"darwin-binary");

        for (platform, bytes) in [("linux-x86_64", &linux), ("darwin-aarch64", &darwin)] {
            registry
                .store_installed_overwrite(
                    native_meta("nativetool", "0.1.0", platform),
                    bytes,
                    &sha256_hex(bytes),
                )
                .unwrap();
        }

        let version_dir = dir.path().join("nativetool/0.1.0");
        let mut files: Vec<String> = fs::read_dir(&version_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "nativetool-0.1.0-darwin-aarch64.meta.json",
                "nativetool-0.1.0-darwin-aarch64.mur.zip",
                "nativetool-0.1.0-darwin-aarch64.sha256",
                "nativetool-0.1.0-linux-x86_64.meta.json",
                "nativetool-0.1.0-linux-x86_64.mur.zip",
                "nativetool-0.1.0-linux-x86_64.sha256",
            ]
        );

        let resolved_linux = registry
            .resolve_with_platform("nativetool", "0.1.0", Some("linux-x86_64"))
            .unwrap();
        let resolved_darwin = registry
            .resolve_with_platform("nativetool", "0.1.0", Some("darwin-aarch64"))
            .unwrap();

        assert_eq!(resolved_linux.platform_match, PlatformMatch::Tagged);
        assert_eq!(resolved_darwin.platform_match, PlatformMatch::Tagged);
        assert_eq!(resolved_linux.bytes, Bytes::from(linux));
        assert_eq!(resolved_darwin.bytes, Bytes::from(darwin));
        assert_ne!(resolved_linux.sha256, resolved_darwin.sha256);
        assert_eq!(
            resolved_linux.meta.platforms,
            vec![("linux".to_string(), "x86_64".to_string())]
        );
        assert_eq!(
            resolved_darwin.meta.platforms,
            vec![("darwin".to_string(), "aarch64".to_string())]
        );
        assert_eq!(resolved_linux.meta.runtime, RuntimeType::Native);
    }

    #[test]
    fn a_platform_independent_payload_stays_at_the_generic_path() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[
            (
                "murmur.yaml",
                b"name: wasmtool\nversion: 0.1.0\nruntime: tool\n",
            ),
            ("tool.wasm", b"\0asm"),
        ]);

        registry
            .store_installed_overwrite(test_meta("wasmtool", "0.1.0"), &bytes, &sha256_hex(&bytes))
            .unwrap();

        assert!(dir
            .path()
            .join("wasmtool/0.1.0/wasmtool-0.1.0.meta.json")
            .exists());
        // Requested for a platform this payload was never tagged with, and still resolved.
        let resolved = registry
            .resolve_with_platform("wasmtool", "0.1.0", Some("darwin-aarch64"))
            .unwrap();
        assert_eq!(resolved.platform_match, PlatformMatch::NotApplicable);
        assert!(resolved.meta.platforms.is_empty());
    }

    #[test]
    fn a_native_payload_off_the_generic_path_reports_the_untagged_fallback() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_artifact_zip("nativetool", "0.1.0", b"binary");
        // An install written before platform tagging: generic paths, `platforms: []`.
        let mut meta = native_meta("nativetool", "0.1.0", "linux-x86_64");
        meta.platforms = Vec::new();
        registry
            .store_installed_overwrite(meta, &bytes, &sha256_hex(&bytes))
            .unwrap();

        let resolved = registry
            .resolve_with_platform("nativetool", "0.1.0", Some("linux-x86_64"))
            .unwrap();
        assert_eq!(resolved.platform_match, PlatformMatch::UntaggedFallback);
        assert_eq!(resolved.bytes, Bytes::from(bytes));
    }

    #[test]
    fn missing_metadata_is_derived_from_the_payload() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_artifact_zip("nativetool", "0.1.0", b"binary");
        let version_dir = dir.path().join("nativetool/0.1.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(
            version_dir.join("nativetool-0.1.0-linux-x86_64.mur.zip"),
            &bytes,
        )
        .unwrap();
        fs::write(
            version_dir.join("nativetool-0.1.0-linux-x86_64.sha256"),
            sha256_hex(&bytes),
        )
        .unwrap();

        let resolved = registry
            .resolve_with_platform("nativetool", "0.1.0", Some("linux-x86_64"))
            .unwrap();
        assert_eq!(resolved.meta.runtime, RuntimeType::Native);
        assert_eq!(resolved.meta.artifact_runtime, "tool");
        assert_eq!(
            resolved.meta.platforms,
            vec![("linux".to_string(), "x86_64".to_string())]
        );
        assert_eq!(resolved.platform_match, PlatformMatch::Tagged);

        // The same derivation backs `mur list`, which has no sidecar to read either.
        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].runtime, RuntimeType::Native);
    }

    #[test]
    fn a_payload_that_does_not_parse_keeps_the_pre_derivation_defaults() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let version_dir = dir.path().join("opaque/0.1.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(version_dir.join("opaque-0.1.0.mur.zip"), b"not-a-zip").unwrap();
        fs::write(
            version_dir.join("opaque-0.1.0.sha256"),
            sha256_hex(b"not-a-zip"),
        )
        .unwrap();

        let resolved = registry.resolve("opaque", "0.1.0").unwrap();
        assert_eq!(resolved.meta.runtime, RuntimeType::Wasm);
        assert!(resolved.meta.artifact_runtime.is_empty());
        assert!(resolved.meta.platforms.is_empty());
    }

    #[test]
    fn list_index_returns_one_row_per_version_with_every_platform_installed() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        for (platform, filler) in [
            ("linux-x86_64", b"linux".as_slice()),
            ("darwin-aarch64", b"darwin".as_slice()),
        ] {
            let bytes = native_artifact_zip("nativetool", "0.1.0", filler);
            registry
                .store_installed_overwrite(
                    native_meta("nativetool", "0.1.0", platform),
                    &bytes,
                    &sha256_hex(&bytes),
                )
                .unwrap();
        }

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(
            index[0].platforms,
            vec![
                ("darwin".to_string(), "aarch64".to_string()),
                ("linux".to_string(), "x86_64".to_string()),
            ]
        );
        assert_eq!(index[0].runtime, RuntimeType::Native);
    }

    #[test]
    fn metadata_written_before_the_field_existed_still_loads() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry
            .publish(test_meta("old", "0.1.0"), b"bytes")
            .unwrap();

        let path = registry.metadata_path_for("old", "0.1.0", None);
        fs::write(
            &path,
            r#"{"meta":{"name":"old","version":"0.1.0","runtime":"wasm","artifact_runtime":"tool","platforms":[],"description":null,"tags":[]}}"#,
        )
        .unwrap();

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert!(index[0].wit_contracts.is_none());
    }

    // ── recorded runtime versus declared runtime ──────────────────────────────

    /// A `.mur.zip` whose manifest declares a native tool.
    fn native_zip(name: &str, version: &str) -> Vec<u8> {
        zip_with(&[
            (
                "murmur.yaml",
                format!(
                    "name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n"
                )
                .as_bytes(),
            ),
            (&format!("bin/{name}"), b"binary"),
        ])
    }

    /// Write one payload, its sha256 sidecar and a `.meta.json` recording exactly
    /// `recorded_runtime` and `recorded_artifact_runtime`, whether or not the payload agrees.
    fn write_store_by_hand(
        registry: &LocalRegistry,
        name: &str,
        version: &str,
        bytes: &[u8],
        recorded_runtime: &str,
        recorded_artifact_runtime: &str,
    ) {
        let dir = registry.artifact_path_for(name, version);
        let dir = dir.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(registry.artifact_path_for(name, version), bytes).unwrap();
        fs::write(registry.sha256_path_for(name, version), sha256_hex(bytes)).unwrap();
        let meta = format!(
            r#"{{"meta":{{"name":"{name}","version":"{version}","runtime":"{recorded_runtime}","artifact_runtime":"{recorded_artifact_runtime}","platforms":[],"description":null,"tags":[]}}}}"#
        );
        fs::write(registry.metadata_path_for(name, version, None), meta).unwrap();
    }

    #[test]
    fn a_native_payload_recorded_as_wasm_resolves_as_native() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_zip("my-tool", "1.0.0");
        write_store_by_hand(&registry, "my-tool", "1.0.0", &bytes, "wasm", "");

        let metadata_path = registry.metadata_path_for("my-tool", "1.0.0", None);
        let on_disk_before = fs::read_to_string(&metadata_path).unwrap();

        let resolved = registry.resolve("my-tool", "1.0.0").unwrap();

        assert_eq!(resolved.meta.runtime, RuntimeType::Native);
        assert_eq!(resolved.meta.artifact_runtime, "tool");
        assert_eq!(
            fs::read_to_string(&metadata_path).unwrap(),
            on_disk_before,
            "resolving must not rewrite the sidecar"
        );
    }

    #[test]
    fn a_recorded_runtime_stands_for_bytes_carrying_no_manifest() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        write_store_by_hand(
            &registry,
            "opaque",
            "1.0.0",
            b"not-an-archive",
            "native",
            "tool",
        );

        let resolved = registry.resolve("opaque", "1.0.0").unwrap();

        assert_eq!(resolved.meta.runtime, RuntimeType::Native);
        assert_eq!(resolved.meta.artifact_runtime, "tool");
    }
}
