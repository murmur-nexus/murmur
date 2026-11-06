use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    artifact::{
        declared_runtime_from_artifact_bytes, load_manifest_from_artifact_bytes,
        load_manifest_yaml_from_artifact_bytes,
    },
    artifact_index::{asset_file_name, metadata_file_name, payload_stem, sha256_file_name},
    artifact_name::ArtifactName,
    build::artifact_name_format_error,
    build_info::{build_info_from_artifact_bytes, BUILD_INFO_FILE},
    manifest::refuse_native_payload,
    wit_contract::{wit_contracts_from_artifact_bytes, WitContracts},
};

pub const RESERVED_VERSIONS: [&str; 3] = ["latest", "stable", "edge"];

/// Registry package implementation type.
///
/// `wasm` marks component bytes; `static` marks a payload with no executable (e.g. a skill
/// guidance file) that is installed for the agent to read directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeType {
    Wasm,
    Static,
}

impl RuntimeType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wasm => "wasm",
            Self::Static => "static",
        }
    }
}

impl FromStr for RuntimeType {
    type Err = RegistryError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.to_ascii_lowercase().as_str() {
            "wasm" => Ok(Self::Wasm),
            "static" => Ok(Self::Static),
            _ => Err(RegistryError::InvalidInput(format!(
                "invalid runtime '{input}' (expected wasm|static)"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    /// The full `<scope>/<name>` the artifact is filed under.
    pub name: ArtifactName,
    pub version: String,
    pub runtime: RuntimeType,
    /// High-level role from the artifact manifest `runtime:` field (e.g. "driver", "tool",
    /// "hook", "skill"). Distinct from `runtime` (RuntimeType), which describes the execution
    /// model (wasm/static).
    pub artifact_runtime: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The versioned WIT interfaces the packed component declares, derived from the artifact
    /// bytes by [`LocalRegistry`] on every write. Absent when the artifact carries no readable
    /// component — a skill, a core module, or a payload that does not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wit_contracts: Option<WitContracts>,
    /// GitHub's numeric id of the account that owned the artifact's scope when it was fetched
    /// from that scope's source. Absent for a payload no source served: one installed from a
    /// file, published locally, or pulled at runtime from another store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<u64>,
    /// The WIT level `mur build` packed in the artifact's `murmur-build.json`, read from the
    /// artifact bytes by [`LocalRegistry`] on every write. Absent when the archive packs none:
    /// built before the label existed, or fitting no level its builder knew.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_level: Option<u64>,
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
    fn resolve(
        &self,
        name: &ArtifactName,
        version: &str,
    ) -> Result<ResolvedArtifact, RegistryError>;

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError>;
    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError>;

    /// Every `name@version` this registry can resolve, one row each: the rows a version-less
    /// runtime pull chooses among. A registry over several stores lists the union.
    fn list_stored(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        self.list_index()
    }

    /// Which store `name@version` resolves from, as a message names it (`project store`,
    /// `global store`), or `None` for a registry that does not name its stores.
    fn held_in(&self, name: &ArtifactName, version: &str) -> Option<String> {
        let _ = (name, version);
        None
    }
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
    pub fn artifact_path_for(&self, name: &ArtifactName, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(asset_file_name(name.name(), version))
    }

    #[must_use]
    pub fn sha256_path_for(&self, name: &ArtifactName, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(sha256_file_name(name.name(), version))
    }

    /// `<root>/<scope>/<name>/<version>`. The file names inside carry the bare name only.
    fn artifact_dir(&self, name: &ArtifactName, version: &str) -> PathBuf {
        self.root.join(name.scope()).join(name.name()).join(version)
    }

    /// Path of the metadata sidecar describing the payload.
    #[must_use]
    pub fn metadata_path_for(&self, name: &ArtifactName, version: &str) -> PathBuf {
        self.artifact_dir(name, version)
            .join(metadata_file_name(name.name(), version))
    }

    /// The top-level directories of this store in the flat layout that predates scopes, sorted:
    /// a `<root>/<d>` holding some `<d>/<version>/<d>-<version>*.mur.zip` or `.sha256`. Nothing
    /// reads them; `mur doctor` reports them as safe to delete. A store that does not exist has
    /// none.
    pub fn old_layout_entries(&self) -> Result<Vec<String>, RegistryError> {
        let mut found = Vec::new();
        for name_dir in read_subdirs(&self.root)? {
            let Some(name) = dir_name(&name_dir) else {
                continue;
            };
            let mut old = false;
            for version_dir in read_subdirs(&name_dir)? {
                let Some(version) = dir_name(&version_dir) else {
                    continue;
                };
                let stem = format!("{name}-{version}");
                old = read_file_names(&version_dir)?.iter().any(|file| {
                    file.strip_prefix(&stem).is_some_and(|tail| {
                        (tail.starts_with('-') || tail.starts_with('.'))
                            && (file.ends_with(".mur.zip") || file.ends_with(".sha256"))
                    })
                });
                if old {
                    break;
                }
            }
            if old {
                found.push(name);
            }
        }
        found.sort_unstable();
        Ok(found)
    }

    pub fn store_installed_overwrite(
        &self,
        meta: ArtifactMeta,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<(), RegistryError> {
        verify_sha256(meta.name.as_str(), &meta.version, bytes, expected_sha256)?;
        self.write_artifact_set(&meta, bytes, expected_sha256, true)
    }

    fn write_artifact_set(
        &self,
        meta: &ArtifactMeta,
        bytes: &[u8],
        sha256: &str,
        overwrite: bool,
    ) -> Result<(), RegistryError> {
        refuse_native_payload(meta.name.as_str(), bytes)
            .map_err(|error| native_tool_error(meta.name.as_str(), &error))?;
        // The bare part a payload is filed under is its tool name in every session that stages
        // it, so it must be the name the artifact declares. A payload with no readable manifest
        // declares nothing to compare, and no models: table to check.
        if let Ok(packed) = load_manifest_from_artifact_bytes(bytes) {
            if packed.name != meta.name.name() {
                return Err(RegistryError::PackedNameMismatch {
                    name: meta.name.to_string(),
                    version: meta.version.clone(),
                    packed: packed.name,
                });
            }
            // The launch refuses the same table with E-RUN-052, so a payload carrying one is
            // never stored.
            if let Ok(yaml) = load_manifest_yaml_from_artifact_bytes(bytes) {
                crate::manifest::parse_model_limits(&yaml).map_err(|error| {
                    RegistryError::MalformedModelsTable {
                        name: meta.name.to_string(),
                        version: meta.version.clone(),
                        reason: error.to_string(),
                    }
                })?;
            }
        }
        // Read before anything is written, so a malformed label leaves the store untouched.
        let built_at_level = build_info_from_artifact_bytes(bytes)
            .map_err(|error| RegistryError::InvalidBuildInfo {
                name: meta.name.to_string(),
                version: meta.version.clone(),
                reason: error.to_string(),
            })?
            .map(|info| info.built_at_level);
        let dir = self.artifact_dir(&meta.name, &meta.version);
        fs::create_dir_all(&dir).map_err(|source| RegistryError::Io {
            path: dir.display().to_string(),
            source,
        })?;

        let artifact_path = self.artifact_path_for(&meta.name, &meta.version);
        let hash_path = self.sha256_path_for(&meta.name, &meta.version);
        let metadata_path = self.metadata_path_for(&meta.name, &meta.version);

        if !overwrite && (artifact_path.exists() || hash_path.exists()) {
            return Err(RegistryError::Conflict {
                name: meta.name.to_string(),
                version: meta.version.clone(),
            });
        }

        // The contracts describe these exact bytes, so they are read from them rather than
        // taken from the caller: a hand-authored value cannot survive a store and so cannot
        // drift from the artifact it describes.
        let mut meta = meta.clone();
        meta.wit_contracts = wit_contracts_from_artifact_bytes(bytes);
        meta.built_at_level = built_at_level;

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
        name: &ArtifactName,
        version: &str,
    ) -> Result<ResolvedArtifact, RegistryError> {
        let artifact_path = self.artifact_path_for(name, version);
        let hash_path = self.sha256_path_for(name, version);
        let metadata_path = self.metadata_path_for(name, version);

        if !artifact_path.exists() || !hash_path.exists() {
            if let Some(refusal) = self.legacy_native_payload(name, version)? {
                return Err(refusal);
            }
            return Err(RegistryError::NotFound {
                name: name.to_string(),
                version: version.to_string(),
            });
        }

        let bytes = fs::read(&artifact_path).map_err(|source| RegistryError::Io {
            path: artifact_path.display().to_string(),
            source,
        })?;
        refuse_native_payload(name.as_str(), &bytes)
            .map_err(|error| native_tool_error(name.as_str(), &error))?;

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
            let stored = read_metadata(&metadata_path).map_err(|source| RegistryError::Io {
                path: metadata_path.display().to_string(),
                source,
            })?;
            if stored.native {
                return Err(RegistryError::NativeTool {
                    name: name.to_string(),
                    field: "runtime".to_string(),
                });
            }
            stored.meta
        } else {
            derived_meta(name, version, &bytes)
        };
        meta.name = name.clone();

        // What an artifact is, is what its own packed murmur.yaml says: a payload readable here
        // overrules the sidecar rather than the other way round, so a value recorded wrongly by
        // an earlier install cannot outlive the next resolve. The recorded value stands only for
        // bytes carrying no readable manifest, which say nothing to prefer over it.
        if let Some(declared) = declared_runtime_from_artifact_bytes(&bytes) {
            meta.runtime = declared.runtime;
            meta.artifact_runtime = declared.artifact_runtime;
        }

        Ok(ResolvedArtifact {
            meta,
            sha256,
            bytes: Bytes::from(bytes),
        })
    }

    /// The refusal for a version directory an older mur filled with a native tool under a
    /// platform-tagged file name (`<name>-<version>-<os>-<arch>.mur.zip`) and no payload at the
    /// generic path, or `None` when the directory holds no such payload.
    fn legacy_native_payload(
        &self,
        name: &ArtifactName,
        version: &str,
    ) -> Result<Option<RegistryError>, RegistryError> {
        let dir = self.artifact_dir(name, version);
        if !dir.is_dir() {
            return Ok(None);
        }
        let tagged_prefix = format!("{}-", payload_stem(name.name(), version));
        for file_name in read_file_names(&dir)? {
            let is_tagged_payload = file_name
                .strip_prefix(&tagged_prefix)
                .is_some_and(|tail| tail.ends_with(".mur.zip"));
            if !is_tagged_payload {
                continue;
            }
            let path = dir.join(&file_name);
            let bytes = fs::read(&path).map_err(|source| RegistryError::Io {
                path: path.display().to_string(),
                source,
            })?;
            if let Err(error) = refuse_native_payload(name.as_str(), &bytes) {
                return Ok(Some(native_tool_error(name.as_str(), &error)));
            }
        }
        Ok(None)
    }
}

impl Registry for LocalRegistry {
    fn resolve(
        &self,
        name: &ArtifactName,
        version: &str,
    ) -> Result<ResolvedArtifact, RegistryError> {
        self.resolve_impl(name, version)
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
        let mut index = Vec::new();
        for scope_dir in read_subdirs(&self.root)? {
            let Some(scope) = dir_name(&scope_dir) else {
                continue;
            };
            if artifact_name_format_error(&scope).is_some() {
                continue;
            }
            for name_dir in read_subdirs(&scope_dir)? {
                let Some(bare) = dir_name(&name_dir) else {
                    continue;
                };
                let Ok(name) = ArtifactName::from_parts(&scope, &bare) else {
                    continue;
                };
                for version_dir in read_subdirs(&name_dir)? {
                    let Some(version) = dir_name(&version_dir) else {
                        continue;
                    };
                    if let Some(meta) = self.index_row(&name, &version, &version_dir)? {
                        index.push(meta);
                    }
                }
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

impl LocalRegistry {
    /// The one `list_index` row for `name@version`, read from `version_dir`, or `None` when the
    /// directory holds no payload.
    fn index_row(
        &self,
        name: &ArtifactName,
        version: &str,
        version_dir: &Path,
    ) -> Result<Option<ArtifactMeta>, RegistryError> {
        // A version directory holds `name-ver.mur.zip` and its sidecar. A directory an older mur
        // filled with a native tool may instead hold `name-ver-<os>-<arch>.mur.zip` files: they
        // are listed so the operator can find and delete them, and resolving them refuses.
        let mut payloads: Vec<PathBuf> = Vec::new();
        let mut sidecars: Vec<PathBuf> = Vec::new();
        let prefix = payload_stem(name.name(), version);
        for file_name in read_file_names(version_dir)? {
            let Some(tail) = file_name.strip_prefix(&prefix) else {
                continue;
            };
            if !tail.is_empty() && !tail.starts_with('-') && !tail.starts_with('.') {
                continue;
            }
            if file_name.ends_with(".mur.zip") {
                payloads.push(version_dir.join(&file_name));
            } else if file_name.ends_with(".meta.json") {
                sidecars.push(version_dir.join(&file_name));
            }
        }

        let has_generic = self.artifact_path_for(name, version).exists()
            && self.sha256_path_for(name, version).exists();
        if !has_generic && payloads.is_empty() {
            return Ok(None);
        }

        let generic_sidecar = self.metadata_path_for(name, version);
        let sidecar = if generic_sidecar.exists() {
            Some(generic_sidecar)
        } else {
            sidecars.first().cloned()
        };
        let mut meta = match sidecar {
            Some(sidecar) => {
                read_metadata(&sidecar)
                    .map_err(|source| RegistryError::Io {
                        path: sidecar.display().to_string(),
                        source,
                    })?
                    .meta
            }
            // No sidecar: read the payload rather than guessing, the same derivation
            // `resolve_impl` makes. Only a directory missing its sidecar pays for this read.
            None => {
                let payload = if has_generic {
                    self.artifact_path_for(name, version)
                } else {
                    payloads[0].clone()
                };
                let bytes = fs::read(&payload).map_err(|source| RegistryError::Io {
                    path: payload.display().to_string(),
                    source,
                })?;
                derived_meta(name, version, &bytes)
            }
        };

        meta.name = name.clone();
        meta.version = version.to_string();
        Ok(Some(meta))
    }
}

/// The subdirectories of `dir`, sorted; none when `dir` does not exist.
fn read_subdirs(dir: &Path) -> Result<Vec<PathBuf>, RegistryError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let io_error = |source| RegistryError::Io {
        path: dir.display().to_string(),
        source,
    };
    let mut dirs = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort_unstable();
    Ok(dirs)
}

/// The file names in `dir`, sorted.
fn read_file_names(dir: &Path) -> Result<Vec<String>, RegistryError> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|source| RegistryError::Io {
            path: dir.display().to_string(),
            source,
        })?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_unstable();
    Ok(names)
}

fn dir_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("artifact {name}@{version} is not in the store")]
    NotFound { name: String, version: String },
    #[error("artifact {name}@{version} already exists")]
    Conflict { name: String, version: String },
    /// The payload's packed `murmur.yaml` declares `implementation: native` or
    /// `execution: native`, or its sidecar records `runtime: native`. `name` is the full
    /// `<scope>/<name>`.
    #[error("{}", crate::manifest::native_tool_message(.name, .field))]
    NativeTool { name: String, field: String },
    /// The payload's packed `murmur.yaml` declares a different name than the bare part of the
    /// full name it was to be filed under.
    #[error(
        "artifact {name}@{version} declares name '{packed}' in its murmur.yaml, so it cannot be \
         filed as {name}"
    )]
    PackedNameMismatch {
        name: String,
        version: String,
        packed: String,
    },
    /// The payload's packed `murmur.yaml` ships a `models:` table that
    /// [`crate::parse_model_limits`] refuses. `reason` names the dotted field at fault.
    #[error(
        "{name}@{version} cannot be stored: its murmur.yaml ships a malformed models: table: \
         {reason}"
    )]
    MalformedModelsTable {
        name: String,
        version: String,
        reason: String,
    },
    #[error("reserved artifact version '{0}' is not allowed")]
    ReservedVersion(String),
    /// The archive packs a `murmur-build.json` that does not hold a non-negative integer
    /// `built_at_level`. `reason` completes "its murmur-build.json …".
    #[error("artifact {name}@{version} cannot be stored: its packed {BUILD_INFO_FILE} {reason}")]
    InvalidBuildInfo {
        name: String,
        version: String,
        reason: String,
    },
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
/// A store seeded by a path that wrote only the zip and its hash, still has to answer what the
/// artifact is. `runtime` and `artifact_runtime` come from the `murmur.yaml` packed inside the
/// bytes, so the answer describes these bytes rather than a default. A payload that does not parse
/// falls back to `wasm` with nothing else filled in, so an opaque payload still resolves.
fn derived_meta(name: &ArtifactName, version: &str, bytes: &[u8]) -> ArtifactMeta {
    let (runtime, artifact_runtime) = declared_runtime_from_artifact_bytes(bytes).map_or_else(
        || (RuntimeType::Wasm, String::new()),
        |declared| (declared.runtime, declared.artifact_runtime),
    );
    ArtifactMeta {
        name: name.clone(),
        version: version.to_string(),
        runtime,
        artifact_runtime,
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: None,
        built_at_level: None,
    }
}

/// A sidecar as read from the store.
struct StoredMetadata {
    meta: ArtifactMeta,
    /// The sidecar records `"runtime": "native"`, as an older mur wrote for a native tool. `meta`
    /// then carries `runtime: wasm` so the row can still be listed; resolving it refuses.
    native: bool,
}

fn read_metadata(path: &Path) -> Result<StoredMetadata, io::Error> {
    let raw = fs::read(path)?;
    let mut value: serde_json::Value = serde_json::from_slice(&raw).map_err(invalid_data)?;
    let runtime = value.pointer_mut("/meta/runtime");
    let native = runtime
        .as_ref()
        .is_some_and(|runtime| runtime.as_str() == Some("native"));
    if native {
        if let Some(runtime) = runtime {
            *runtime = serde_json::Value::String(RuntimeType::Wasm.as_str().to_string());
        }
    }
    let metadata: IndexMetadata = serde_json::from_value(value).map_err(invalid_data)?;
    Ok(StoredMetadata {
        meta: metadata.meta,
        native,
    })
}

/// The registry form of a [`crate::ManifestError::NativeTool`] refusal; any other manifest error
/// is reported under the `implementation` field it was raised for.
fn native_tool_error(name: &str, error: &crate::ManifestError) -> RegistryError {
    let field = match error {
        crate::ManifestError::NativeTool { field, .. } => field,
        _ => "implementation",
    };
    RegistryError::NativeTool {
        name: name.to_string(),
        field: (*field).to_string(),
    }
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

/// SHA-256 of `bytes` as 64 lowercase hex characters.
///
/// This is the exact form `murmur.lock` pins and the store's `.sha256` sidecars hold: any change to
/// the output invalidates every existing lock and store, and the FIPS 180-2 known-answer tests in
/// this file pin it. The engine is `ring`, whose SHA-256 selects SIMD or SHA-extension code at run
/// time. Staging hashes every artifact payload and compiled form on each launch, and a pure-Rust
/// SHA-256 built at the release profile's `opt-level = "z"` hashes several times slower.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    lowercase_hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

/// SHA-256 of everything `reader` yields, returned with the number of bytes read.
///
/// The streaming companion to [`sha256_hex`]: same engine, same output, pinned by the same
/// known-answer tests. A caller hashing a file too large to hold in memory uses this rather than a
/// second hasher, so its digest matches the ones `murmur.lock` and the `.sha256` sidecars hold.
pub fn sha256_hex_of_reader(reader: &mut impl std::io::Read) -> std::io::Result<(u64, String)> {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
        total = total.saturating_add(read as u64);
    }
    Ok((total, lowercase_hex(context.finish().as_ref())))
}

fn lowercase_hex(digest: &[u8]) -> String {
    use std::fmt::Write;

    let mut hex = String::with_capacity(2 * digest.len());
    for byte in digest {
        // Writing into a `String` cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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

    /// `ana/<bare>`: the scope these tests file everything under.
    fn n(bare: &str) -> ArtifactName {
        ArtifactName::from_parts("ana", bare).unwrap()
    }

    fn test_meta(name: &str, version: &str) -> ArtifactMeta {
        ArtifactMeta {
            name: n(name),
            version: version.to_string(),
            runtime: RuntimeType::Wasm,
            artifact_runtime: "wasm".to_string(),
            description: Some("demo".to_string()),
            tags: vec!["test".to_string()],
            wit_contracts: None,
            owner_id: None,
            built_at_level: None,
        }
    }

    #[test]
    fn publish_writes_expected_artifact_and_sha_sidecar() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = test_meta("hello", "0.0.2");
        let bytes = b"artifact-contents";

        let published = registry.publish(meta.clone(), bytes).unwrap();
        assert_eq!(published.artifact_id, "ana/hello@0.0.2");

        let artifact_path = dir.path().join("ana/hello/0.0.2/hello-0.0.2.mur.zip");
        let sha_path = dir.path().join("ana/hello/0.0.2/hello-0.0.2.sha256");

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

        let resolved = registry.resolve(&n("hello"), "0.0.2").unwrap();
        assert_eq!(resolved.bytes, Bytes::from_static(bytes));
        assert_eq!(resolved.meta.name, "ana/hello");
        assert_eq!(resolved.sha256, sha256_hex(bytes));
    }

    #[test]
    fn resolve_missing_returns_not_found() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());

        let err = registry.resolve(&n("missing"), "1.0.0").unwrap_err();
        assert!(matches!(
            err,
            RegistryError::NotFound { name, version } if name == "ana/missing" && version == "1.0.0"
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
            RegistryError::Conflict { name, version } if name == "ana/hello" && version == "0.0.2"
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

        let resolved = registry.resolve(&n("hello"), "0.0.2").unwrap();
        assert_eq!(resolved.bytes, Bytes::from_static(new_bytes));
        assert_eq!(resolved.sha256, new_sha);
    }

    #[test]
    fn publish_failure_leaves_no_partial_files() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let meta = test_meta("hello", "0.0.2");

        let target_dir = dir.path().join("ana/hello/0.0.2");
        fs::create_dir_all(&target_dir).unwrap();

        let original_perms = fs::metadata(&target_dir).unwrap().permissions();
        let mut read_only_perms = original_perms.clone();
        read_only_perms.set_readonly(true);
        fs::set_permissions(&target_dir, read_only_perms).unwrap();

        let _ = registry.publish(meta, b"bytes").unwrap_err();

        let artifact_path = dir.path().join("ana/hello/0.0.2/hello-0.0.2.mur.zip");
        let sha_path = dir.path().join("ana/hello/0.0.2/hello-0.0.2.sha256");
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
        assert_eq!(index[0].name.as_str(), "ana/a");
        assert_eq!(index[1].name.as_str(), "ana/b");
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
            name: n("my-skill"),
            version: "0.1.0".to_string(),
            runtime: RuntimeType::Static,
            artifact_runtime: "skill".to_string(),
            description: None,
            tags: Vec::new(),
            wit_contracts: None,
            owner_id: None,
            built_at_level: None,
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
            .resolve(&n("ct"), "0.1.0")
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
            .resolve(&n("ct"), "0.1.0")
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
            .resolve(&n("skill"), "0.1.0")
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

            let raw = fs::read_to_string(registry.metadata_path_for(&n(name), "0.1.0")).unwrap();
            assert!(
                !raw.contains("wit_contracts"),
                "{name}: metadata carries a wit_contracts key: {raw}"
            );
            assert!(registry
                .resolve(&n(name), "0.1.0")
                .unwrap()
                .meta
                .wit_contracts
                .is_none());
        }
    }

    // ── native payloads ───────────────────────────────────────────────────────

    /// A `.mur.zip` whose packed manifest declares a native tool.
    fn native_zip(name: &str, version: &str) -> Vec<u8> {
        let manifest =
            format!("name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n");
        zip_with(&[
            ("murmur.yaml", manifest.as_bytes()),
            (&format!("bin/{name}"), b"binary"),
        ])
    }

    fn assert_native_refusal(error: &RegistryError, field: &str) {
        assert!(
            matches!(
                error,
                RegistryError::NativeTool { name, field: got }
                    if name == "ana/my-tool" && got == field
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "artifact 'ana/my-tool' declares {field}: native; murmur runs tools only as WASM \
                 components"
            )
        );
    }

    #[test]
    fn a_native_payload_at_the_generic_path_is_refused_on_resolve() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_zip("my-tool", "1.0.0");
        let version_dir = dir.path().join("ana/my-tool/1.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(version_dir.join("my-tool-1.0.0.mur.zip"), &bytes).unwrap();
        fs::write(version_dir.join("my-tool-1.0.0.sha256"), sha256_hex(&bytes)).unwrap();

        let error = registry.resolve(&n("my-tool"), "1.0.0").unwrap_err();
        assert_native_refusal(&error, "implementation");
    }

    /// The layout an older mur left for a native tool: the payload only at its platform-tagged
    /// path, beside a sidecar recording `runtime: native` and its platform.
    #[test]
    fn a_tagged_native_install_an_older_mur_left_is_refused_and_still_listed() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_zip("my-tool", "1.0.0");
        let version_dir = dir.path().join("ana/my-tool/1.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(
            version_dir.join("my-tool-1.0.0-linux-x86_64.mur.zip"),
            &bytes,
        )
        .unwrap();
        fs::write(
            version_dir.join("my-tool-1.0.0-linux-x86_64.sha256"),
            sha256_hex(&bytes),
        )
        .unwrap();
        fs::write(
            version_dir.join("my-tool-1.0.0-linux-x86_64.meta.json"),
            r#"{"meta":{"name":"ana/my-tool","version":"1.0.0","runtime":"native","artifact_runtime":"tool","platforms":[["linux","x86_64"]],"description":null,"tags":[]}}"#,
        )
        .unwrap();

        let error = registry.resolve(&n("my-tool"), "1.0.0").unwrap_err();
        assert_native_refusal(&error, "implementation");

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].name.as_str(), "ana/my-tool");
        assert_eq!(index[0].artifact_runtime, "tool");
    }

    /// A sidecar recording `runtime: native` beside bytes carrying no readable manifest is
    /// refused under the field it records.
    #[test]
    fn a_sidecar_recording_runtime_native_is_refused() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let version_dir = dir.path().join("ana/my-tool/1.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(version_dir.join("my-tool-1.0.0.mur.zip"), b"not-an-archive").unwrap();
        fs::write(
            version_dir.join("my-tool-1.0.0.sha256"),
            sha256_hex(b"not-an-archive"),
        )
        .unwrap();
        fs::write(
            version_dir.join("my-tool-1.0.0.meta.json"),
            r#"{"meta":{"name":"ana/my-tool","version":"1.0.0","runtime":"native","artifact_runtime":"tool","platforms":[],"description":null,"tags":[]}}"#,
        )
        .unwrap();

        let error = registry.resolve(&n("my-tool"), "1.0.0").unwrap_err();
        assert_native_refusal(&error, "runtime");
        assert_eq!(registry.list_index().unwrap().len(), 1);
    }

    #[test]
    fn a_native_payload_is_never_stored() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = native_zip("my-tool", "1.0.0");

        let error = registry
            .publish(test_meta("my-tool", "1.0.0"), &bytes)
            .unwrap_err();
        assert_native_refusal(&error, "implementation");
        let error = registry
            .store_installed_overwrite(test_meta("my-tool", "1.0.0"), &bytes, &sha256_hex(&bytes))
            .unwrap_err();
        assert_native_refusal(&error, "implementation");

        assert!(!dir.path().join("ana").exists());
    }

    #[test]
    fn a_wasm_payload_is_filed_at_the_generic_path() {
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

        assert_eq!(
            files_in(&dir.path().join("ana/wasmtool/0.1.0")),
            vec![
                "wasmtool-0.1.0.meta.json",
                "wasmtool-0.1.0.mur.zip",
                "wasmtool-0.1.0.sha256",
            ]
        );
        let resolved = registry.resolve(&n("wasmtool"), "0.1.0").unwrap();
        assert_eq!(resolved.bytes, Bytes::from(bytes));
    }

    #[test]
    fn missing_metadata_is_derived_from_the_payload() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[(
            "murmur.yaml",
            b"name: skilly\nversion: 0.1.0\nruntime: skill\n",
        )]);
        let version_dir = dir.path().join("ana/skilly/0.1.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(version_dir.join("skilly-0.1.0.mur.zip"), &bytes).unwrap();
        fs::write(version_dir.join("skilly-0.1.0.sha256"), sha256_hex(&bytes)).unwrap();

        let resolved = registry.resolve(&n("skilly"), "0.1.0").unwrap();
        assert_eq!(resolved.meta.runtime, RuntimeType::Static);
        assert_eq!(resolved.meta.artifact_runtime, "skill");

        // The same derivation backs `mur list`, which has no sidecar to read either.
        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].runtime, RuntimeType::Static);
    }

    #[test]
    fn a_payload_that_does_not_parse_keeps_the_pre_derivation_defaults() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let version_dir = dir.path().join("ana/opaque/0.1.0");
        fs::create_dir_all(&version_dir).unwrap();
        fs::write(version_dir.join("opaque-0.1.0.mur.zip"), b"not-a-zip").unwrap();
        fs::write(
            version_dir.join("opaque-0.1.0.sha256"),
            sha256_hex(b"not-a-zip"),
        )
        .unwrap();

        let resolved = registry.resolve(&n("opaque"), "0.1.0").unwrap();
        assert_eq!(resolved.meta.runtime, RuntimeType::Wasm);
        assert!(resolved.meta.artifact_runtime.is_empty());
    }

    #[test]
    fn metadata_written_before_the_field_existed_still_loads() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry
            .publish(test_meta("old", "0.1.0"), b"bytes")
            .unwrap();

        let path = registry.metadata_path_for(&n("old"), "0.1.0");
        fs::write(
            &path,
            r#"{"meta":{"name":"ana/old","version":"0.1.0","runtime":"wasm","artifact_runtime":"tool","platforms":[],"description":null,"tags":[]}}"#,
        )
        .unwrap();

        let index = registry.list_index().unwrap();
        assert_eq!(index.len(), 1);
        assert!(index[0].wit_contracts.is_none());
        assert!(index[0].owner_id.is_none());
    }

    #[test]
    fn the_owner_id_is_written_beside_the_payload_only_when_known() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[
            (
                "murmur.yaml",
                b"name: wasmtool\nversion: 0.1.0\nruntime: tool\n",
            ),
            ("tool.wasm", b"\0asm"),
        ]);
        let sha256 = sha256_hex(&bytes);
        let path = registry.metadata_path_for(&n("wasmtool"), "0.1.0");

        registry
            .store_installed_overwrite(test_meta("wasmtool", "0.1.0"), &bytes, &sha256)
            .unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("owner_id"));

        let mut meta = test_meta("wasmtool", "0.1.0");
        meta.owner_id = Some(1111);
        registry
            .store_installed_overwrite(meta, &bytes, &sha256)
            .unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"owner_id\": 1111"));
        let resolved = registry.resolve(&n("wasmtool"), "0.1.0").unwrap();
        assert_eq!(resolved.meta.owner_id, Some(1111));
    }

    #[test]
    fn the_built_at_level_is_read_from_the_packed_build_info_on_every_write() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let manifest: (&str, &[u8]) = (
            "murmur.yaml",
            b"name: wasmtool\nversion: 0.1.0\nruntime: tool\n",
        );
        let path = registry.metadata_path_for(&n("wasmtool"), "0.1.0");

        let unlabelled = zip_with(&[manifest]);
        let mut meta = test_meta("wasmtool", "0.1.0");
        meta.built_at_level = Some(9);
        registry
            .store_installed_overwrite(meta.clone(), &unlabelled, &sha256_hex(&unlabelled))
            .unwrap();
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("built_at_level"));

        let labelled = zip_with(&[manifest, (BUILD_INFO_FILE, b"{\"built_at_level\":1}\n")]);
        registry
            .store_installed_overwrite(meta.clone(), &labelled, &sha256_hex(&labelled))
            .unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"built_at_level\": 1"));
        let resolved = registry.resolve(&n("wasmtool"), "0.1.0").unwrap();
        assert_eq!(resolved.meta.built_at_level, Some(1));

        let malformed = zip_with(&[manifest, (BUILD_INFO_FILE, b"{\"built_at_level\":-1}")]);
        let error = registry
            .store_installed_overwrite(meta, &malformed, &sha256_hex(&malformed))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "artifact ana/wasmtool@0.1.0 cannot be stored: its packed murmur-build.json sets \
             built_at_level to -1, which is not a non-negative integer"
        );
        assert_eq!(
            registry.resolve(&n("wasmtool"), "0.1.0").unwrap().bytes,
            labelled
        );
    }

    // ── recorded runtime versus declared runtime ──────────────────────────────

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
        let dir = registry.artifact_path_for(&n(name), version);
        let dir = dir.parent().unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(registry.artifact_path_for(&n(name), version), bytes).unwrap();
        fs::write(
            registry.sha256_path_for(&n(name), version),
            sha256_hex(bytes),
        )
        .unwrap();
        let meta = format!(
            r#"{{"meta":{{"name":"ana/{name}","version":"{version}","runtime":"{recorded_runtime}","artifact_runtime":"{recorded_artifact_runtime}","description":null,"tags":[]}}}}"#
        );
        fs::write(registry.metadata_path_for(&n(name), version), meta).unwrap();
    }

    #[test]
    fn a_skill_payload_recorded_as_wasm_resolves_as_static() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[(
            "murmur.yaml",
            b"name: my-skill\nversion: 1.0.0\nruntime: skill\n",
        )]);
        write_store_by_hand(&registry, "my-skill", "1.0.0", &bytes, "wasm", "");

        let metadata_path = registry.metadata_path_for(&n("my-skill"), "1.0.0");
        let on_disk_before = fs::read_to_string(&metadata_path).unwrap();

        let resolved = registry.resolve(&n("my-skill"), "1.0.0").unwrap();

        assert_eq!(resolved.meta.runtime, RuntimeType::Static);
        assert_eq!(resolved.meta.artifact_runtime, "skill");
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
            "static",
            "skill",
        );

        let resolved = registry.resolve(&n("opaque"), "1.0.0").unwrap();

        assert_eq!(resolved.meta.runtime, RuntimeType::Static);
        assert_eq!(resolved.meta.artifact_runtime, "skill");
    }

    // ── scoped layout ─────────────────────────────────────────────────────────

    fn meta_in(scope: &str, bare: &str, version: &str) -> ArtifactMeta {
        ArtifactMeta {
            name: ArtifactName::from_parts(scope, bare).unwrap(),
            ..test_meta(bare, version)
        }
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut files: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn registry_lists_one_bare_name_in_two_scopes_as_two_rows_sorted_by_full_name() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry
            .publish(meta_in("bob", "tool-x", "1.0.0"), b"bob")
            .unwrap();
        registry
            .publish(meta_in("ana", "tool-x", "1.0.0"), b"ana")
            .unwrap();

        let index = registry.list_index().unwrap();
        let names: Vec<&str> = index.iter().map(|meta| meta.name.as_str()).collect();
        assert_eq!(names, ["ana/tool-x", "bob/tool-x"]);

        let ana = registry.resolve(&n("tool-x"), "1.0.0").unwrap();
        let bob = registry
            .resolve(&ArtifactName::parse("bob/tool-x").unwrap(), "1.0.0")
            .unwrap();
        assert_eq!(ana.bytes, Bytes::from_static(b"ana"));
        assert_eq!(bob.bytes, Bytes::from_static(b"bob"));
    }

    #[test]
    fn registry_refuses_a_payload_packed_under_another_name_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[
            (
                "murmur.yaml",
                b"name: other\nversion: 1.0.0\nruntime: tool\n",
            ),
            ("tool.wasm", b"\0asm"),
        ]);

        let error = registry
            .publish(test_meta("tool-x", "1.0.0"), &bytes)
            .unwrap_err();
        assert!(
            matches!(
                &error,
                RegistryError::PackedNameMismatch { name, version, packed }
                    if name == "ana/tool-x" && version == "1.0.0" && packed == "other"
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "artifact ana/tool-x@1.0.0 declares name 'other' in its murmur.yaml, so it cannot be \
             filed as ana/tool-x"
        );

        let error = registry
            .store_installed_overwrite(test_meta("tool-x", "1.0.0"), &bytes, &sha256_hex(&bytes))
            .unwrap_err();
        assert!(
            matches!(error, RegistryError::PackedNameMismatch { .. }),
            "{error:?}"
        );
        assert!(files_in(dir.path()).is_empty(), "the store was written to");
    }

    #[test]
    fn registry_refuses_a_payload_with_a_malformed_models_table_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let bytes = zip_with(&[
            (
                "murmur.yaml",
                b"name: tool-x\nversion: 1.0.0\nruntime: driver\nmodels:\n  m:\n    \
                  max_output_tokens: 128000\n    context_window: 1050000\n    \
                  max_input_tokens: 1050001\n",
            ),
            ("tool.wasm", b"\0asm"),
        ]);

        let error = registry
            .publish(test_meta("tool-x", "1.0.0"), &bytes)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ana/tool-x@1.0.0 cannot be stored: its murmur.yaml ships a malformed models: table: \
             murmur.yaml: field 'models.m.max_input_tokens' is 1050001, above this entry's context_window of \
             1050000; an input ceiling is at most the window"
        );
        let error = registry
            .store_installed_overwrite(test_meta("tool-x", "1.0.0"), &bytes, &sha256_hex(&bytes))
            .unwrap_err();
        assert!(
            matches!(error, RegistryError::MalformedModelsTable { .. }),
            "{error:?}"
        );
        assert!(files_in(dir.path()).is_empty(), "the store was written to");
    }

    #[test]
    fn registry_stores_a_payload_with_no_packed_manifest_as_before() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        let no_manifest = zip_with(&[("tool.wasm", b"\0asm")]);

        registry
            .publish(test_meta("tool-x", "1.0.0"), &no_manifest)
            .unwrap();
        registry
            .publish(test_meta("opaque", "1.0.0"), b"not-a-zip")
            .unwrap();

        assert!(dir
            .path()
            .join("ana/tool-x/1.0.0/tool-x-1.0.0.mur.zip")
            .exists());
        assert!(dir
            .path()
            .join("ana/opaque/1.0.0/opaque-1.0.0.mur.zip")
            .exists());
        let resolved = registry.resolve(&n("tool-x"), "1.0.0").unwrap();
        assert_eq!(resolved.bytes, Bytes::from(no_manifest));
    }

    #[test]
    fn registry_list_index_skips_a_directory_whose_name_fails_the_part_rule() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        registry
            .publish(test_meta("tool-x", "1.0.0"), b"good")
            .unwrap();
        for (scope, bare) in [("Bad_Scope", "x"), ("ana", "Bad_Name"), ("-ana", "x")] {
            let version_dir = dir.path().join(scope).join(bare).join("1.0.0");
            fs::create_dir_all(&version_dir).unwrap();
            fs::write(version_dir.join(format!("{bare}-1.0.0.mur.zip")), b"bad").unwrap();
            fs::write(
                version_dir.join(format!("{bare}-1.0.0.sha256")),
                sha256_hex(b"bad"),
            )
            .unwrap();
        }

        let index = registry.list_index().unwrap();
        let names: Vec<&str> = index.iter().map(|meta| meta.name.as_str()).collect();
        assert_eq!(names, ["ana/tool-x"]);
    }

    #[test]
    fn old_layout_entries_list_the_flat_store_and_nothing_else() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());
        assert!(registry.old_layout_entries().unwrap().is_empty());

        let old_dir = dir.path().join("murmur-tool-git/0.2.0");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("murmur-tool-git-0.2.0.mur.zip"), b"old").unwrap();
        fs::write(
            old_dir.join("murmur-tool-git-0.2.0.sha256"),
            sha256_hex(b"old"),
        )
        .unwrap();
        registry
            .publish(meta_in("local", "echo-tool", "0.0.1"), b"new")
            .unwrap();

        assert_eq!(
            registry.old_layout_entries().unwrap(),
            vec!["murmur-tool-git".to_string()]
        );
        let index = registry.list_index().unwrap();
        let names: Vec<&str> = index.iter().map(|meta| meta.name.as_str()).collect();
        assert_eq!(names, ["local/echo-tool"]);
    }

    #[test]
    fn old_layout_entries_of_a_missing_store_are_empty() {
        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path().join("absent"));
        assert!(registry.old_layout_entries().unwrap().is_empty());
    }

    /// The FIPS 180-2 SHA-256 vectors short enough to write as literals, as `(name, input, digest)`.
    const FIPS_180_2_VECTORS: [(&str, &[u8], &str); 4] = [
        (
            "empty",
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            "abc",
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            "448-bit message",
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (
            "896-bit message",
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
        ),
    ];

    const MILLION_A_LEN: usize = 1_000_000;
    const MILLION_A_SHA256: &str =
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";

    /// Yields at most `chunk` bytes per `read`, so a digest spans many partial reads and never
    /// lines up with the 64-byte SHA-256 block or the reader function's own buffer.
    struct TrickleReader<R> {
        inner: R,
        chunk: usize,
    }

    impl<R: std::io::Read> std::io::Read for TrickleReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let len = buf.len().min(self.chunk);
            self.inner.read(&mut buf[..len])
        }
    }

    #[test]
    fn sha256_hex_matches_the_fips_180_2_vectors() {
        for (name, input, expected) in FIPS_180_2_VECTORS {
            assert_eq!(sha256_hex(input), expected, "FIPS 180-2 vector {name:?}");
        }

        let million_a = vec![b'a'; MILLION_A_LEN];
        assert_eq!(
            sha256_hex(&million_a),
            MILLION_A_SHA256,
            "FIPS 180-2 vector \"one million 'a'\""
        );
    }

    #[test]
    fn sha256_hex_of_reader_matches_the_fips_180_2_vectors_across_partial_reads() {
        use std::io::Read;

        for (name, input, expected) in FIPS_180_2_VECTORS {
            let want = (input.len() as u64, expected.to_string());
            assert_eq!(
                sha256_hex_of_reader(&mut std::io::Cursor::new(input)).unwrap(),
                want,
                "FIPS 180-2 vector {name:?} through a Cursor"
            );
            let mut trickle = TrickleReader {
                inner: input,
                chunk: 7,
            };
            assert_eq!(
                sha256_hex_of_reader(&mut trickle).unwrap(),
                want,
                "FIPS 180-2 vector {name:?} through 7-byte partial reads"
            );
        }

        let want = (MILLION_A_LEN as u64, MILLION_A_SHA256.to_string());
        assert_eq!(
            sha256_hex_of_reader(&mut std::io::repeat(b'a').take(MILLION_A_LEN as u64)).unwrap(),
            want,
            "FIPS 180-2 vector \"one million 'a'\" through io::repeat"
        );
        let mut trickle = TrickleReader {
            inner: std::io::repeat(b'a').take(MILLION_A_LEN as u64),
            chunk: 7,
        };
        assert_eq!(
            sha256_hex_of_reader(&mut trickle).unwrap(),
            want,
            "FIPS 180-2 vector \"one million 'a'\" through 7-byte partial reads"
        );
    }
}
