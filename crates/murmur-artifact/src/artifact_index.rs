//! The artifact index a source publishes beside its release assets: every version it serves of
//! every artifact, each with the payload files that make it up, their SHA-256, and the `murmur:`
//! interfaces the version's component names.
//!
//! [`index_directory`] derives an index from the `.mur.zip` files themselves; only the scope and
//! the release tag say where the files live. [`ArtifactIndex::from_json`] is the one reader, and
//! [`ArtifactIndex::to_json`] the one writer: its output is a pure function of the index value, so
//! indexing the same files twice gives the same bytes.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use semver::Version;
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    artifact::ArtifactError, artifact_name_format_error, build_info_from_artifact_bytes,
    is_reserved_version, level_fit, load_manifest_from_artifact_bytes, newest_fitting_level,
    sha256_hex, split_platform_suffix, wit_contracts_from_artifact_bytes, LevelFit, RuntimeType,
    UnservedInterface, WitContracts, WitLevel, BUILD_INFO_FILE, LOCAL_SCOPE, SUPPORTED_PLATFORMS,
};

/// The `schema_version` [`ArtifactIndex::from_json`] reads and [`ArtifactIndex::to_json`] writes.
pub const ARTIFACT_INDEX_SCHEMA_VERSION: u64 = 2;

/// The file name a source publishes its index under, and `mur index`'s default output.
pub const ARTIFACT_INDEX_FILE_NAME: &str = "artifacts-index.json";

/// The extension every payload file carries.
pub const ARTIFACT_FILE_SUFFIX: &str = ".mur.zip";

/// The namespace of the interfaces an index records. `wasi:` imports link against any
/// semver-compatible version the host provides, so they never decide whether a version fits.
pub const MURMUR_INTERFACE_PREFIX: &str = "murmur:";

/// One source's index: [`Self::artifacts`] sorted by name, each artifact's versions newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactIndex {
    pub schema_version: u64,
    /// The scope every artifact in the index is named under.
    pub scope: String,
    pub artifacts: Vec<IndexedArtifact>,
}

/// One artifact. The artifact-level fields describe its newest version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedArtifact {
    /// The bare name, as packed in `murmur.yaml`.
    pub name: String,
    /// The packed `runtime:` role (`tool`, `hook`, `driver`, `skill`, …).
    pub runtime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// Newest first by semver precedence.
    pub versions: Vec<IndexedVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedVersion {
    pub version: String,
    /// The release tag whose assets hold [`Self::assets`].
    pub release: String,
    /// The WIT level the version was built at. `mur index` never derives it; `--merge` carries
    /// it over from the earlier index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_at_level: Option<u64>,
    /// Every `murmur:` interface the version's component exports or imports, sorted and
    /// deduplicated across its assets.
    pub interfaces: Vec<String>,
    /// Sorted by file name.
    pub assets: Vec<IndexedAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexedAsset {
    /// `<name>-<version>[-<platform>].mur.zip`.
    pub file: String,
    /// Present exactly when [`Self::file`] carries a platform tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// [`sha256_hex`] of the file's bytes.
    pub sha256: String,
}

/// Why a JSON document is not an index this reader accepts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArtifactIndexError {
    #[error("is not valid JSON: {0}")]
    Malformed(String),
    #[error("is a schema 1 index")]
    Schema1,
    #[error("has schema_version {0}, and this mur reads only schema 2")]
    UnsupportedSchema(String),
    /// `at` locates the offending value, e.g. `artifacts[2].versions[0].assets`.
    #[error("is not a valid schema 2 index: {at} {reason}")]
    Invalid { at: String, reason: String },
}

/// Why [`index_directory`] or [`merge_indexes`] wrote no index.
#[derive(Debug, Error)]
pub enum IndexBuildError {
    #[error("cannot read the directory {dir}: {source}")]
    ReadDir {
        dir: String,
        #[source]
        source: io::Error,
    },
    #[error("{dir} holds no {ARTIFACT_FILE_SUFFIX} file to index")]
    NoArtifacts { dir: String },
    #[error("{file} cannot be indexed: {reason}")]
    File { file: String, reason: String },
    #[error("'{scope}' is not a usable index scope: it {reason}")]
    InvalidScope { scope: String, reason: String },
    #[error("'{release}' is not a usable release tag: it {reason}")]
    InvalidRelease {
        release: String,
        reason: &'static str,
    },
    #[error("the index being merged has scope '{previous}', and these files are indexed for scope '{scope}'")]
    ScopeMismatch { previous: String, scope: String },
}

/// What [`merge_indexes`] did with the earlier index's versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Versions only the earlier index holds, carried over verbatim.
    pub kept: usize,
    /// Versions both hold whose asset files or digests differ, written from the new files.
    pub replaced: usize,
}

/// Whether one version can run on a host serving a given WIT level and platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionFit {
    Fits,
    /// Its interfaces whose version is not the one the level lists for their package, in
    /// recorded order. Never empty.
    Unserved(Vec<UnservedInterface>),
    /// Every payload is tagged for a platform other than this one.
    NoPayloadFor(String),
}

impl ArtifactIndex {
    /// Read an index, checking `schema_version` before anything else, then every constraint of
    /// the schema 2 format: part-rule names and scope, unique sorted artifacts, versions that
    /// parse as semver newest first, `murmur:`-only sorted interfaces, and assets whose file
    /// names agree with their artifact, version and platform. Keys the format does not define
    /// are ignored.
    pub fn from_json(text: &str) -> Result<Self, ArtifactIndexError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| ArtifactIndexError::Malformed(error.to_string()))?;
        let root = value
            .as_object()
            .ok_or_else(|| invalid("the document", "is not a JSON object"))?;
        match root.get("schema_version") {
            Some(Value::Number(n)) if n.as_u64() == Some(ARTIFACT_INDEX_SCHEMA_VERSION) => {}
            Some(Value::Number(n)) if n.as_u64() == Some(1) => {
                return Err(ArtifactIndexError::Schema1)
            }
            Some(Value::String(s)) if s == "1" => return Err(ArtifactIndexError::Schema1),
            Some(other) => return Err(ArtifactIndexError::UnsupportedSchema(other.to_string())),
            None => return Err(invalid("schema_version", "is missing")),
        }
        read_index(root)
    }

    /// The index as `serde_json` pretty JSON with one trailing newline.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|error| unreachable!("an index always serializes: {error}"));
        text.push('\n');
        text
    }

    /// The number of versions across every artifact.
    #[must_use]
    pub fn version_count(&self) -> usize {
        self.artifacts.iter().map(|a| a.versions.len()).sum()
    }
}

impl IndexedArtifact {
    /// The newest version [`IndexedVersion::fit`] accepts for `level` and `platform`.
    #[must_use]
    pub fn newest_fitting(&self, level: &WitLevel, platform: &str) -> Option<&IndexedVersion> {
        self.versions
            .iter()
            .find(|version| version.fit(level, platform) == VersionFit::Fits)
    }
}

impl IndexedVersion {
    #[must_use]
    pub fn fit(&self, level: &WitLevel, platform: &str) -> VersionFit {
        version_fit(
            &self.interfaces,
            self.assets.iter().map(|asset| asset.platform.as_deref()),
            level,
            platform,
        )
    }
}

/// A version fits when its `interfaces` fit `level` ([`level_fit`]) and one of its payloads is
/// untagged (`None`) or tagged `platform`. The interface check comes first, so a version failing
/// both reports [`VersionFit::Unserved`].
pub fn version_fit<'a>(
    interfaces: &[String],
    payload_platforms: impl IntoIterator<Item = Option<&'a str>>,
    level: &WitLevel,
    platform: &str,
) -> VersionFit {
    if let LevelFit::Mismatched(unserved) = level_fit(&interfaces_as_contracts(interfaces), level) {
        return VersionFit::Unserved(unserved);
    }
    let runs_here = payload_platforms
        .into_iter()
        .any(|tag| tag.is_none() || tag == Some(platform));
    if runs_here {
        VersionFit::Fits
    } else {
        VersionFit::NoPayloadFor(platform.to_string())
    }
}

/// An index version's `interfaces`, which merge both directions, as contracts [`level_fit`]
/// reads. Fit treats exports and imports alike, so which list they go in does not change it.
fn interfaces_as_contracts(interfaces: &[String]) -> WitContracts {
    WitContracts {
        exports: Vec::new(),
        imports: interfaces.to_vec(),
    }
}

/// The built-at level of an index version that packs no label: the newest of `levels` its
/// `interfaces` fit, `None` when they fit none.
#[must_use]
pub fn derived_built_at_level(interfaces: &[String], levels: &[WitLevel]) -> Option<u64> {
    newest_fitting_level(&interfaces_as_contracts(interfaces), levels)
}

/// The `murmur:` names among `contracts`' exports and imports, sorted and deduplicated.
#[must_use]
pub fn murmur_interfaces(contracts: &WitContracts) -> Vec<String> {
    contracts.matching_prefix(MURMUR_INTERFACE_PREFIX)
}

/// Why `scope` cannot head an index, if it cannot: it must pass the part rule and must not be
/// [`LOCAL_SCOPE`], which no source serves.
#[must_use]
pub fn index_scope_error(scope: &str) -> Option<String> {
    if let Some(reason) = artifact_name_format_error(scope) {
        return Some(reason);
    }
    (scope == LOCAL_SCOPE).then(|| {
        format!("is '{LOCAL_SCOPE}', which names artifacts installed from a .mur.zip file")
    })
}

/// Why `tag` cannot be recorded as a version's `release`, if it cannot.
#[must_use]
pub fn release_tag_error(tag: &str) -> Option<&'static str> {
    if tag.is_empty() {
        Some("is empty")
    } else if tag.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("contains whitespace or a control character")
    } else {
        None
    }
}

/// The asset file name for one payload: `<name>-<version>[-<platform>].mur.zip`.
#[must_use]
pub fn asset_file_name(name: &str, version: &str, platform: Option<&str>) -> String {
    match platform {
        Some(platform) => format!("{name}-{version}-{platform}{ARTIFACT_FILE_SUFFIX}"),
        None => format!("{name}-{version}{ARTIFACT_FILE_SUFFIX}"),
    }
}

/// The first sentence of a manifest `description:`: its first non-empty line, trimmed, cut after
/// the first `.`, `!` or `?` that ends the line or is followed by whitespace. `None` when no line
/// has text.
#[must_use]
pub fn description_summary(description: &str) -> Option<String> {
    let line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|(_, next)| next.is_whitespace())
        {
            return Some(line[..at + c.len_utf8()].to_string());
        }
    }
    Some(line.to_string())
}

/// The search tags for an artifact: the role tags of its `runtime`, then each `-`-separated word
/// of its bare name except `murmur`, skipping words already present.
#[must_use]
pub fn derived_tags(runtime: &str, name: &str) -> Vec<String> {
    let mut tags: Vec<String> = match runtime {
        "driver" => vec!["driver".into(), "inference".into()],
        other => vec![other.to_string()],
    };
    for word in name.split('-') {
        if word.is_empty() || word == "murmur" || tags.iter().any(|tag| tag == word) {
            continue;
        }
        tags.push(word.to_string());
    }
    tags
}

/// One `.mur.zip` file read for indexing.
struct ScannedFile {
    file: String,
    path: String,
    name: String,
    version: String,
    runtime: String,
    packaging: RuntimeType,
    platform: Option<&'static str>,
    sha256: String,
    interfaces: Vec<String>,
    description: Option<String>,
    built_at_level: Option<u64>,
}

/// Index every `.mur.zip` regular file directly inside `dir`, recording `release` on every
/// version. Every other entry is ignored.
///
/// Each version's `built_at_level` is the label its files pack in `murmur-build.json`; a version
/// whose files pack none gets the newest of `levels` its `interfaces` fit, and none when they fit
/// no level.
///
/// Refuses the whole directory when any one file cannot be indexed (unreadable, no packed
/// `murmur.yaml`, a malformed packed `murmur-build.json`, a name or version the registry would
/// refuse, a file name that disagrees with the packed name, version or packaging type), when two
/// files of one version disagree on their packed `runtime:`, packaging type or built-at level, or
/// when `dir` holds no such file. The result depends only on the files' names and bytes and on
/// `levels`, never on directory order or file times.
pub fn index_directory(
    dir: &Path,
    scope: &str,
    release: &str,
    levels: &[WitLevel],
) -> Result<ArtifactIndex, IndexBuildError> {
    if let Some(reason) = index_scope_error(scope) {
        return Err(IndexBuildError::InvalidScope {
            scope: scope.to_string(),
            reason,
        });
    }
    if let Some(reason) = release_tag_error(release) {
        return Err(IndexBuildError::InvalidRelease {
            release: release.to_string(),
            reason,
        });
    }

    let read_dir_error = |source| IndexBuildError::ReadDir {
        dir: dir.display().to_string(),
        source,
    };
    let mut file_names = Vec::new();
    for entry in fs::read_dir(dir).map_err(read_dir_error)? {
        let entry = entry.map_err(read_dir_error)?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if file_name.ends_with(ARTIFACT_FILE_SUFFIX)
            && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_file())
        {
            file_names.push(file_name);
        }
    }
    if file_names.is_empty() {
        return Err(IndexBuildError::NoArtifacts {
            dir: dir.display().to_string(),
        });
    }
    file_names.sort();

    // name → version → files of that version, in file-name order.
    let mut grouped: BTreeMap<String, BTreeMap<String, Vec<ScannedFile>>> = BTreeMap::new();
    for file_name in file_names {
        let scanned = scan_file(dir, file_name)?;
        let files = grouped
            .entry(scanned.name.clone())
            .or_default()
            .entry(scanned.version.clone())
            .or_default();
        let labelled = files
            .iter()
            .find(|file| file.built_at_level.is_some() && scanned.built_at_level.is_some());
        if let Some(first) = labelled.filter(|file| file.built_at_level != scanned.built_at_level) {
            return Err(IndexBuildError::File {
                file: scanned.path,
                reason: format!(
                    "its packed {BUILD_INFO_FILE} sets built_at_level {}, and {} of the same \
                     {}@{} sets built_at_level {}",
                    scanned.built_at_level.unwrap_or_default(),
                    first.path,
                    first.name,
                    first.version,
                    first.built_at_level.unwrap_or_default()
                ),
            });
        }
        if let Some(first) = files.first() {
            if first.runtime != scanned.runtime || first.packaging != scanned.packaging {
                return Err(IndexBuildError::File {
                    file: scanned.path,
                    reason: format!(
                        "it packs runtime '{}' ({}), and {} of the same {}@{} packs runtime '{}' \
                         ({})",
                        scanned.runtime,
                        scanned.packaging.as_str(),
                        first.path,
                        first.name,
                        first.version,
                        first.runtime,
                        first.packaging.as_str()
                    ),
                });
            }
        }
        files.push(scanned);
    }

    let artifacts = grouped
        .into_iter()
        .map(|(name, versions)| {
            let mut versions: Vec<(String, Vec<ScannedFile>)> = versions.into_iter().collect();
            versions.sort_by(|(a, _), (b, _)| compare_versions(b, a));
            let newest = &versions[0].1[0];
            let runtime = newest.runtime.clone();
            let description = newest.description.as_deref().and_then(description_summary);
            let tags = derived_tags(&runtime, &name);
            let versions = versions
                .into_iter()
                .map(|(version, files)| {
                    let mut interfaces: Vec<String> = files
                        .iter()
                        .flat_map(|file| file.interfaces.iter().cloned())
                        .collect();
                    interfaces.sort();
                    interfaces.dedup();
                    let built_at_level = files
                        .iter()
                        .find_map(|file| file.built_at_level)
                        .or_else(|| derived_built_at_level(&interfaces, levels));
                    IndexedVersion {
                        version,
                        release: release.to_string(),
                        built_at_level,
                        interfaces,
                        assets: files
                            .into_iter()
                            .map(|file| IndexedAsset {
                                file: file.file,
                                platform: file.platform.map(str::to_string),
                                sha256: file.sha256,
                            })
                            .collect(),
                    }
                })
                .collect();
            IndexedArtifact {
                name,
                runtime,
                description,
                tags,
                versions,
            }
        })
        .collect();

    Ok(ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        scope: scope.to_string(),
        artifacts,
    })
}

fn scan_file(dir: &Path, file: String) -> Result<ScannedFile, IndexBuildError> {
    let path = dir.join(&file).display().to_string();
    let refuse = |reason: String| IndexBuildError::File {
        file: path.clone(),
        reason,
    };

    let bytes = fs::read(dir.join(&file)).map_err(|e| refuse(format!("it cannot be read: {e}")))?;
    let manifest = load_manifest_from_artifact_bytes(&bytes).map_err(|error| {
        refuse(match error {
            ArtifactError::Zip { source, .. } => format!("it is not a zip archive ({source})"),
            ArtifactError::MissingManifest => "it holds no packed murmur.yaml".to_string(),
            ArtifactError::Manifest(error) => format!("its packed murmur.yaml is invalid: {error}"),
            other => format!("its packed murmur.yaml cannot be read: {other}"),
        })
    })?;

    let name = manifest.name.clone();
    if let Some(reason) = artifact_name_format_error(&name) {
        return Err(refuse(format!("its packed name '{name}' {reason}")));
    }
    let version = manifest.version.clone();
    if is_reserved_version(&version) {
        return Err(refuse(format!(
            "its packed version '{version}' is reserved and names no single release"
        )));
    }
    if let Err(error) = Version::parse(&version) {
        return Err(refuse(format!(
            "its packed version '{version}' is not a semantic version ({error})"
        )));
    }

    let stem = format!("{name}-{version}");
    let platform = match split_platform_suffix(&file) {
        Some((head, platform)) if head == stem => Some(platform),
        _ if file == asset_file_name(&name, &version, None) => None,
        _ => {
            return Err(refuse(format!(
                "its file name does not match its packed name '{name}' and version '{version}': \
                 expected {stem}{ARTIFACT_FILE_SUFFIX}, or {stem}-<platform>{ARTIFACT_FILE_SUFFIX} \
                 with <platform> one of {}",
                SUPPORTED_PLATFORMS.join(", ")
            )))
        }
    };

    let packaging = manifest.registry_runtime();
    match (packaging, platform) {
        (RuntimeType::Native, None) => {
            return Err(refuse(format!(
                "a native payload names its platform: {stem}-<platform>{ARTIFACT_FILE_SUFFIX}"
            )))
        }
        (RuntimeType::Wasm | RuntimeType::Static, Some(_)) => {
            return Err(refuse(format!(
                "a {} payload runs on every platform; name it {}",
                packaging.as_str(),
                asset_file_name(&name, &version, None)
            )))
        }
        _ => {}
    }

    let interfaces = wit_contracts_from_artifact_bytes(&bytes)
        .map(|contracts| murmur_interfaces(&contracts))
        .unwrap_or_default();
    let built_at_level = build_info_from_artifact_bytes(&bytes)
        .map_err(|error| refuse(format!("its packed {BUILD_INFO_FILE} {error}")))?
        .map(|info| info.built_at_level);

    Ok(ScannedFile {
        sha256: sha256_hex(&bytes),
        file,
        path,
        name,
        version,
        runtime: manifest.runtime,
        packaging,
        platform,
        interfaces,
        description: manifest.description,
        built_at_level,
    })
}

/// Merge `fresh`, indexed from a release's files, over `previous`, an earlier index of the same
/// scope.
///
/// Every version `fresh` holds is taken from it. A version both hold with the same assets keeps
/// `previous`'s `built_at_level` when it has one; one whose assets differ is counted as replaced.
/// Every other version is kept verbatim, except that one with no `built_at_level` gets the newest
/// of `levels` its `interfaces` fit. An artifact's own fields come from whichever index holds its
/// newest version, `fresh` on a tie.
pub fn merge_indexes(
    fresh: ArtifactIndex,
    previous: ArtifactIndex,
    levels: &[WitLevel],
) -> Result<(ArtifactIndex, MergeReport), IndexBuildError> {
    if fresh.scope != previous.scope {
        return Err(IndexBuildError::ScopeMismatch {
            previous: previous.scope,
            scope: fresh.scope,
        });
    }

    let mut report = MergeReport::default();
    let mut merged: BTreeMap<String, IndexedArtifact> = BTreeMap::new();
    let mut earlier: BTreeMap<String, IndexedArtifact> = previous
        .artifacts
        .into_iter()
        .map(|artifact| (artifact.name.clone(), artifact))
        .collect();

    for mut artifact in fresh.artifacts {
        let Some(old) = earlier.remove(&artifact.name) else {
            merged.insert(artifact.name.clone(), artifact);
            continue;
        };
        let old_is_newer =
            compare_versions(&old.versions[0].version, &artifact.versions[0].version)
                == Ordering::Greater;
        for old_version in old.versions.iter() {
            match artifact
                .versions
                .iter_mut()
                .find(|version| version.version == old_version.version)
            {
                Some(version) if version.assets == old_version.assets => {
                    if old_version.built_at_level.is_some() {
                        version.built_at_level = old_version.built_at_level;
                    }
                }
                Some(_) => report.replaced += 1,
                None => {
                    report.kept += 1;
                    artifact
                        .versions
                        .push(kept_version(old_version.clone(), levels));
                }
            }
        }
        artifact
            .versions
            .sort_by(|a, b| compare_versions(&b.version, &a.version));
        if old_is_newer {
            artifact.runtime = old.runtime;
            artifact.description = old.description;
            artifact.tags = old.tags;
        }
        merged.insert(artifact.name.clone(), artifact);
    }
    for (name, mut artifact) in earlier {
        report.kept += artifact.versions.len();
        artifact.versions = artifact
            .versions
            .into_iter()
            .map(|version| kept_version(version, levels))
            .collect();
        merged.insert(name, artifact);
    }

    Ok((
        ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            scope: fresh.scope,
            artifacts: merged.into_values().collect(),
        },
        report,
    ))
}

/// A version carried over from an earlier index, given the derived built-at level when it records
/// none.
fn kept_version(mut version: IndexedVersion, levels: &[WitLevel]) -> IndexedVersion {
    if version.built_at_level.is_none() {
        version.built_at_level = derived_built_at_level(&version.interfaces, levels);
    }
    version
}

/// Semver precedence, then build metadata, so two distinct versions never compare equal. A
/// string that is not semver sorts below every one that is, and two such strings compare as
/// text: a store files any version string that is not reserved.
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    Version::parse(a)
        .ok()
        .cmp(&Version::parse(b).ok())
        .then_with(|| a.cmp(b))
}

// ── Reading ───────────────────────────────────────────────────────────────────

fn invalid(at: impl Into<String>, reason: impl Into<String>) -> ArtifactIndexError {
    ArtifactIndexError::Invalid {
        at: at.into(),
        reason: reason.into(),
    }
}

fn child(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_string()
    } else {
        format!("{at}.{key}")
    }
}

fn object<'a>(value: &'a Value, at: &str) -> Result<&'a Map<String, Value>, ArtifactIndexError> {
    value
        .as_object()
        .ok_or_else(|| invalid(at, "is not a JSON object"))
}

fn required<'a>(
    obj: &'a Map<String, Value>,
    at: &str,
    key: &str,
) -> Result<&'a Value, ArtifactIndexError> {
    obj.get(key)
        .ok_or_else(|| invalid(child(at, key), "is missing"))
}

fn string(value: &Value, at: &str) -> Result<String, ArtifactIndexError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid(at, "is not a string"))
}

fn required_string(
    obj: &Map<String, Value>,
    at: &str,
    key: &str,
) -> Result<String, ArtifactIndexError> {
    string(required(obj, at, key)?, &child(at, key))
}

fn optional_string(
    obj: &Map<String, Value>,
    at: &str,
    key: &str,
) -> Result<Option<String>, ArtifactIndexError> {
    obj.get(key)
        .map(|value| string(value, &child(at, key)))
        .transpose()
}

fn required_array<'a>(
    obj: &'a Map<String, Value>,
    at: &str,
    key: &str,
) -> Result<&'a Vec<Value>, ArtifactIndexError> {
    required(obj, at, key)?
        .as_array()
        .ok_or_else(|| invalid(child(at, key), "is not an array"))
}

fn string_array(
    obj: &Map<String, Value>,
    at: &str,
    key: &str,
) -> Result<Vec<String>, ArtifactIndexError> {
    let at = child(at, key);
    let items = obj
        .get(key)
        .ok_or_else(|| invalid(&at, "is missing"))?
        .as_array()
        .ok_or_else(|| invalid(&at, "is not an array"))?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| string(item, &format!("{at}[{i}]")))
        .collect()
}

fn read_index(root: &Map<String, Value>) -> Result<ArtifactIndex, ArtifactIndexError> {
    let scope = required_string(root, "", "scope")?;
    if let Some(reason) = index_scope_error(&scope) {
        return Err(invalid("scope", format!("'{scope}' {reason}")));
    }
    let mut artifacts: Vec<IndexedArtifact> = Vec::new();
    for (i, value) in required_array(root, "", "artifacts")?.iter().enumerate() {
        let at = format!("artifacts[{i}]");
        let artifact = read_artifact(value, &at)?;
        if let Some(prev) = artifacts.last() {
            if prev.name.as_bytes() >= artifact.name.as_bytes() {
                return Err(invalid(
                    child(&at, "name"),
                    format!(
                        "'{}' does not sort after '{}'; artifacts are sorted by name and names are \
                         unique",
                        artifact.name, prev.name
                    ),
                ));
            }
        }
        artifacts.push(artifact);
    }
    Ok(ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        scope,
        artifacts,
    })
}

fn read_artifact(value: &Value, at: &str) -> Result<IndexedArtifact, ArtifactIndexError> {
    let obj = object(value, at)?;
    let name = required_string(obj, at, "name")?;
    if let Some(reason) = artifact_name_format_error(&name) {
        return Err(invalid(child(at, "name"), format!("'{name}' {reason}")));
    }
    let runtime = required_string(obj, at, "runtime")?;
    if runtime.is_empty() {
        return Err(invalid(child(at, "runtime"), "is empty"));
    }
    let description = optional_string(obj, at, "description")?;
    if description.as_deref() == Some("") {
        return Err(invalid(child(at, "description"), "is empty"));
    }
    let tags = string_array(obj, at, "tags")?;
    for (i, tag) in tags.iter().enumerate() {
        if tags[..i].contains(tag) {
            return Err(invalid(
                format!("{at}.tags[{i}]"),
                format!("repeats '{tag}'"),
            ));
        }
    }

    let values = required_array(obj, at, "versions")?;
    if values.is_empty() {
        return Err(invalid(child(at, "versions"), "is empty"));
    }
    let mut versions: Vec<IndexedVersion> = Vec::new();
    for (j, value) in values.iter().enumerate() {
        let at = format!("{at}.versions[{j}]");
        let version = read_version(value, &at, &name)?;
        if let Some(prev) = versions.last() {
            if compare_versions(&prev.version, &version.version) != Ordering::Greater {
                return Err(invalid(
                    child(&at, "version"),
                    format!(
                        "'{}' is not older than '{}'; versions are unique and newest first",
                        version.version, prev.version
                    ),
                ));
            }
        }
        versions.push(version);
    }

    Ok(IndexedArtifact {
        name,
        runtime,
        description,
        tags,
        versions,
    })
}

fn read_version(value: &Value, at: &str, name: &str) -> Result<IndexedVersion, ArtifactIndexError> {
    let obj = object(value, at)?;
    let version = required_string(obj, at, "version")?;
    if is_reserved_version(&version) {
        return Err(invalid(
            child(at, "version"),
            format!("'{version}' is reserved"),
        ));
    }
    if let Err(error) = Version::parse(&version) {
        return Err(invalid(
            child(at, "version"),
            format!("'{version}' is not a semantic version ({error})"),
        ));
    }
    let release = required_string(obj, at, "release")?;
    if let Some(reason) = release_tag_error(&release) {
        return Err(invalid(child(at, "release"), reason));
    }
    let built_at_level = match obj.get("built_at_level") {
        None => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            invalid(child(at, "built_at_level"), "is not a non-negative integer")
        })?),
    };

    let interfaces = string_array(obj, at, "interfaces")?;
    for (i, interface) in interfaces.iter().enumerate() {
        let at = format!("{at}.interfaces[{i}]");
        if !interface.starts_with(MURMUR_INTERFACE_PREFIX) {
            return Err(invalid(
                at,
                format!("'{interface}' is not a {MURMUR_INTERFACE_PREFIX} interface"),
            ));
        }
        if i > 0 && interfaces[i - 1] >= *interface {
            return Err(invalid(
                at,
                format!(
                    "'{interface}' does not sort after '{}'; interfaces are sorted and unique",
                    interfaces[i - 1]
                ),
            ));
        }
    }

    let values = required_array(obj, at, "assets")?;
    if values.is_empty() {
        return Err(invalid(child(at, "assets"), "is empty"));
    }
    let mut assets: Vec<IndexedAsset> = Vec::new();
    for (k, value) in values.iter().enumerate() {
        let at = format!("{at}.assets[{k}]");
        let asset = read_asset(value, &at, name, &version)?;
        if let Some(prev) = assets.last() {
            if prev.file >= asset.file {
                return Err(invalid(
                    child(&at, "file"),
                    format!(
                        "'{}' does not sort after '{}'; assets are sorted by file and unique",
                        asset.file, prev.file
                    ),
                ));
            }
        }
        assets.push(asset);
    }

    Ok(IndexedVersion {
        version,
        release,
        built_at_level,
        interfaces,
        assets,
    })
}

fn read_asset(
    value: &Value,
    at: &str,
    name: &str,
    version: &str,
) -> Result<IndexedAsset, ArtifactIndexError> {
    let obj = object(value, at)?;
    let file = required_string(obj, at, "file")?;
    let platform = optional_string(obj, at, "platform")?;
    if let Some(platform) = &platform {
        if !SUPPORTED_PLATFORMS.contains(&platform.as_str()) {
            return Err(invalid(
                child(at, "platform"),
                format!(
                    "'{platform}' is not one of {}",
                    SUPPORTED_PLATFORMS.join(", ")
                ),
            ));
        }
    }
    let expected = asset_file_name(name, version, platform.as_deref());
    if file != expected {
        return Err(invalid(
            child(at, "file"),
            format!("'{file}' is not '{expected}', the file name of this artifact, version and platform"),
        ));
    }
    let sha256 = required_string(obj, at, "sha256")?;
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid(
            child(at, "sha256"),
            "is not 64 lowercase hexadecimal characters",
        ));
    }
    Ok(IndexedAsset {
        file,
        platform,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::*;

    fn levels() -> Vec<WitLevel> {
        crate::parse_wit_levels("1 murmur:hook@0.9.0\n1 murmur:tool@0.1.0\n").unwrap()
    }

    fn served() -> WitLevel {
        levels().remove(0)
    }

    fn payload(manifest: &str, component: Option<&str>) -> Vec<u8> {
        labelled(manifest, component, None)
    }

    /// A payload that also packs `label` as its `murmur-build.json`.
    fn labelled(manifest: &str, component: Option<&str>, label: Option<&str>) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = ZipWriter::new(Cursor::new(&mut buf));
            zip.start_file("murmur.yaml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            if let Some(wat) = component {
                zip.start_file("capsule.wasm", SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(&wat::parse_str(wat).unwrap()).unwrap();
            }
            if let Some(label) = label {
                zip.start_file(BUILD_INFO_FILE, SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(label.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    fn importing(names: &[&str]) -> String {
        let imports: String = names
            .iter()
            .map(|name| format!("(import \"{name}\" (instance))"))
            .collect();
        format!("(component {imports})")
    }

    fn sha(fill: char) -> String {
        fill.to_string().repeat(64)
    }

    fn version(version: &str, release: &str, assets: &[(&str, char)]) -> IndexedVersion {
        IndexedVersion {
            version: version.to_string(),
            release: release.to_string(),
            built_at_level: None,
            interfaces: vec![],
            assets: assets
                .iter()
                .map(|(file, fill)| IndexedAsset {
                    file: (*file).to_string(),
                    platform: split_platform_suffix(file).map(|(_, p)| p.to_string()),
                    sha256: sha(*fill),
                })
                .collect(),
        }
    }

    fn artifact(name: &str, versions: Vec<IndexedVersion>) -> IndexedArtifact {
        IndexedArtifact {
            name: name.to_string(),
            runtime: "tool".to_string(),
            description: None,
            tags: derived_tags("tool", name),
            versions,
        }
    }

    fn index(artifacts: Vec<IndexedArtifact>) -> ArtifactIndex {
        ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            scope: "ana".to_string(),
            artifacts,
        }
    }

    #[test]
    fn versions_order_by_semver_then_text() {
        let mut versions = vec!["0.9.0", "dev", "0.10.0", "abc", "1.0.0-rc.1"];
        versions.sort_by(|a, b| compare_versions(b, a));
        assert_eq!(versions, ["1.0.0-rc.1", "0.10.0", "0.9.0", "dev", "abc"]);
    }

    #[test]
    fn description_summary_takes_the_first_sentence_of_the_first_line() {
        assert_eq!(
            description_summary("\n  Read, write and patch files. Also more.\nSecond line."),
            Some("Read, write and patch files.".to_string())
        );
        assert_eq!(
            description_summary("Version 1.2 ships! Next"),
            Some("Version 1.2 ships!".to_string())
        );
        assert_eq!(
            description_summary("Does it work?"),
            Some("Does it work?".to_string())
        );
        assert_eq!(
            description_summary("no terminator here"),
            Some("no terminator here".to_string())
        );
        assert_eq!(description_summary(" \n \n"), None);
    }

    #[test]
    fn derived_tags_follow_the_role_then_the_name_words() {
        assert_eq!(derived_tags("tool", "tool-editor"), ["tool", "editor"]);
        assert_eq!(
            derived_tags("driver", "murmur-driver-anthropic"),
            ["driver", "inference", "anthropic"]
        );
        assert_eq!(derived_tags("hook", "murmur-hook-x"), ["hook", "x"]);
        assert_eq!(derived_tags("skill", "skill"), ["skill"]);
        assert_eq!(derived_tags("recipe", "a-b-a"), ["recipe", "a", "b"]);
    }

    #[test]
    fn schema_1_is_named_whether_a_string_or_a_number() {
        for text in [
            r#"{"schema_version":"1","updated_at":"x","artifacts":[]}"#,
            r#"{"schema_version":1,"artifacts":[]}"#,
        ] {
            assert_eq!(
                ArtifactIndex::from_json(text),
                Err(ArtifactIndexError::Schema1)
            );
        }
        assert_eq!(
            ArtifactIndex::from_json(r#"{"schema_version":99}"#),
            Err(ArtifactIndexError::UnsupportedSchema("99".to_string()))
        );
        assert_eq!(
            ArtifactIndex::from_json(r#"{"schema_version":"2"}"#),
            Err(ArtifactIndexError::UnsupportedSchema("\"2\"".to_string()))
        );
        assert!(matches!(
            ArtifactIndex::from_json("{not json"),
            Err(ArtifactIndexError::Malformed(_))
        ));
    }

    #[test]
    fn a_written_index_reads_back_equal() {
        let mut v = version("0.1.0", "v0.1.0", &[("tool-x-0.1.0.mur.zip", 'a')]);
        v.built_at_level = Some(1);
        v.interfaces = vec!["murmur:tool/run@0.1.0".to_string()];
        let written = index(vec![artifact(
            "tool-x",
            vec![
                version(
                    "0.2.0",
                    "v0.2.0",
                    &[
                        ("tool-x-0.2.0-darwin-aarch64.mur.zip", 'b'),
                        ("tool-x-0.2.0-linux-x86_64.mur.zip", 'c'),
                    ],
                ),
                v,
            ],
        )]);
        let text = written.to_json();
        assert!(text.ends_with("}\n") && !text.ends_with("\n\n"));
        assert_eq!(ArtifactIndex::from_json(&text).unwrap(), written);
    }

    #[test]
    fn invalid_structure_names_the_json_location() {
        let mut value: Value = serde_json::from_str(
            &index(vec![artifact(
                "tool-x",
                vec![version("0.1.0", "v1", &[("tool-x-0.1.0.mur.zip", 'a')])],
            )])
            .to_json(),
        )
        .unwrap();
        value["artifacts"][0]["versions"][0]
            .as_object_mut()
            .unwrap()
            .remove("assets");
        let err = ArtifactIndex::from_json(&value.to_string()).unwrap_err();
        assert_eq!(
            err,
            invalid("artifacts[0].versions[0].assets", "is missing"),
            "{err}"
        );
    }

    #[test]
    fn the_reader_refuses_what_the_format_forbids() {
        let good = index(vec![artifact(
            "tool-x",
            vec![
                version("0.2.0", "v1", &[("tool-x-0.2.0.mur.zip", 'a')]),
                version("0.1.0", "v1", &[("tool-x-0.1.0.mur.zip", 'a')]),
            ],
        )]);
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("scope", Box::new(|v| v["scope"] = "local".into())),
            ("scope", Box::new(|v| v["scope"] = "Ana".into())),
            (
                "artifacts[0].versions[1].version",
                Box::new(|v| {
                    let versions = v["artifacts"][0]["versions"].as_array_mut().unwrap();
                    versions.reverse();
                }),
            ),
            (
                "artifacts[0].versions[0].version",
                Box::new(|v| v["artifacts"][0]["versions"][0]["version"] = "latest".into()),
            ),
            (
                "artifacts[0].versions[0].assets[0].sha256",
                Box::new(|v| {
                    v["artifacts"][0]["versions"][0]["assets"][0]["sha256"] = "A".repeat(64).into()
                }),
            ),
            (
                "artifacts[0].versions[0].assets[0].file",
                Box::new(|v| {
                    v["artifacts"][0]["versions"][0]["assets"][0]["file"] =
                        "tool-y-0.2.0.mur.zip".into()
                }),
            ),
            (
                "artifacts[0].versions[0].interfaces[0]",
                Box::new(|v| {
                    v["artifacts"][0]["versions"][0]["interfaces"] =
                        serde_json::json!(["wasi:cli/run@0.2.0"])
                }),
            ),
            (
                "artifacts[0].versions[0].built_at_level",
                Box::new(|v| v["artifacts"][0]["versions"][0]["built_at_level"] = (-1).into()),
            ),
            (
                "artifacts[0].versions[0].release",
                Box::new(|v| v["artifacts"][0]["versions"][0]["release"] = "v 1".into()),
            ),
        ];
        for (at, mutate) in cases {
            let mut value: Value = serde_json::from_str(&good.to_json()).unwrap();
            mutate(&mut value);
            match ArtifactIndex::from_json(&value.to_string()) {
                Err(ArtifactIndexError::Invalid { at: got, .. }) => assert_eq!(got, at),
                other => panic!("expected Invalid at {at}, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut value: Value = serde_json::from_str(
            &index(vec![artifact(
                "tool-x",
                vec![version("0.1.0", "v1", &[("tool-x-0.1.0.mur.zip", 'a')])],
            )])
            .to_json(),
        )
        .unwrap();
        value["later"] = true.into();
        value["artifacts"][0]["versions"][0]["later"] = 1.into();
        assert!(ArtifactIndex::from_json(&value.to_string()).is_ok());
    }

    #[test]
    fn fit_checks_interfaces_then_platform() {
        let mut v = version(
            "1.0.0",
            "v1",
            &[("tool-x-1.0.0-darwin-aarch64.mur.zip", 'a')],
        );
        assert_eq!(
            v.fit(&served(), "linux-x86_64"),
            VersionFit::NoPayloadFor("linux-x86_64".to_string())
        );
        assert_eq!(v.fit(&served(), "darwin-aarch64"), VersionFit::Fits);
        v.interfaces = vec!["murmur:tool/run@9.9.9".to_string()];
        match v.fit(&served(), "linux-x86_64") {
            VersionFit::Unserved(unserved) => {
                assert_eq!(unserved[0].served_version.as_deref(), Some("0.1.0"))
            }
            other => panic!("{other:?}"),
        }
        let untagged = version("1.0.0", "v1", &[("tool-x-1.0.0.mur.zip", 'a')]);
        assert_eq!(untagged.fit(&served(), "linux-aarch64"), VersionFit::Fits);
    }

    #[test]
    fn merge_keeps_old_versions_and_counts_replacements() {
        let mut kept = version("0.1.0", "v0.1.0", &[("tool-x-0.1.0.mur.zip", 'a')]);
        kept.built_at_level = Some(1);
        let previous = index(vec![
            artifact(
                "tool-gone",
                vec![version(
                    "1.0.0",
                    "v0.1.0",
                    &[("tool-gone-1.0.0.mur.zip", 'e')],
                )],
            ),
            artifact(
                "tool-x",
                vec![
                    version("0.2.0", "v0.2.0", &[("tool-x-0.2.0.mur.zip", 'b')]),
                    kept.clone(),
                ],
            ),
        ]);
        let fresh = index(vec![artifact(
            "tool-x",
            vec![
                version("0.3.0", "v0.3.0", &[("tool-x-0.3.0.mur.zip", 'c')]),
                version("0.2.0", "v0.3.0", &[("tool-x-0.2.0.mur.zip", 'd')]),
            ],
        )]);
        let (merged, report) = merge_indexes(fresh, previous, &levels()).unwrap();
        assert_eq!(
            report,
            MergeReport {
                kept: 2,
                replaced: 1
            }
        );
        assert_eq!(merged.artifacts[0].name, "tool-gone");
        let versions = &merged.artifacts[1].versions;
        let listed: Vec<_> = versions
            .iter()
            .map(|v| (v.version.as_str(), v.release.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("0.3.0", "v0.3.0"),
                ("0.2.0", "v0.3.0"),
                ("0.1.0", "v0.1.0")
            ]
        );
        assert_eq!(versions[2], kept);
        // A kept version recording no level gets the derived one.
        assert_eq!(merged.artifacts[0].versions[0].built_at_level, Some(1));
        assert!(ArtifactIndex::from_json(&merged.to_json()).is_ok());
    }

    #[test]
    fn merge_keeps_artifact_fields_from_the_index_holding_the_newest_version() {
        let mut newer = artifact(
            "tool-x",
            vec![version("2.0.0", "v2", &[("tool-x-2.0.0.mur.zip", 'a')])],
        );
        newer.description = Some("Old but newest.".to_string());
        let mut older = artifact(
            "tool-x",
            vec![version("1.0.0", "v1", &[("tool-x-1.0.0.mur.zip", 'b')])],
        );
        older.description = Some("Fresh but older.".to_string());
        let (merged, _) = merge_indexes(index(vec![older]), index(vec![newer]), &levels()).unwrap();
        assert_eq!(
            merged.artifacts[0].description.as_deref(),
            Some("Old but newest.")
        );
    }

    #[test]
    fn merge_refuses_another_scope() {
        let mut previous = index(vec![]);
        previous.scope = "bob".to_string();
        assert!(matches!(
            merge_indexes(index(vec![]), previous, &levels()),
            Err(IndexBuildError::ScopeMismatch { .. })
        ));
    }

    #[test]
    fn index_directory_derives_every_field_from_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let tool = |v: &str| {
            payload(
                &format!(
                    "name: tool-echo\nversion: {v}\nruntime: tool\ndescription: |\n  Echo it back. Twice.\n"
                ),
                Some(&importing(&["murmur:tool/run@0.1.0", "wasi:cli/environment@0.2.0"])),
            )
        };
        fs::write(dir.path().join("tool-echo-0.10.0.mur.zip"), tool("0.10.0")).unwrap();
        fs::write(dir.path().join("tool-echo-0.9.0.mur.zip"), tool("0.9.0")).unwrap();
        let native = payload(
            "name: tool-nat\nversion: 1.0.0\nruntime: tool\nimplementation: native\n",
            None,
        );
        for platform in ["linux-x86_64", "darwin-aarch64"] {
            fs::write(
                dir.path()
                    .join(format!("tool-nat-1.0.0-{platform}.mur.zip")),
                &native,
            )
            .unwrap();
        }
        fs::write(dir.path().join("notes.txt"), "x").unwrap();
        fs::create_dir(dir.path().join("sub.mur.zip")).unwrap();

        let built = index_directory(dir.path(), "ana", "v1.0.0", &levels()).unwrap();
        assert_eq!(built.artifacts.len(), 2);
        let echo = &built.artifacts[0];
        assert_eq!(echo.name, "tool-echo");
        assert_eq!(echo.description.as_deref(), Some("Echo it back."));
        assert_eq!(echo.tags, ["tool", "echo"]);
        assert_eq!(echo.versions[0].version, "0.10.0");
        assert_eq!(echo.versions[0].interfaces, ["murmur:tool/run@0.1.0"]);
        assert_eq!(echo.versions[0].built_at_level, Some(1));
        assert_eq!(
            echo.versions[0].assets[0].sha256,
            sha256_hex(&tool("0.10.0"))
        );
        let nat = &built.artifacts[1];
        assert_eq!(nat.versions.len(), 1);
        let platforms: Vec<_> = nat.versions[0]
            .assets
            .iter()
            .map(|a| a.platform.as_deref())
            .collect();
        assert_eq!(platforms, [Some("darwin-aarch64"), Some("linux-x86_64")]);
        assert!(nat.versions[0].interfaces.is_empty());
        assert_eq!(nat.versions[0].built_at_level, Some(1));
        assert_eq!(ArtifactIndex::from_json(&built.to_json()).unwrap(), built);
    }

    #[test]
    fn index_directory_refuses_a_file_that_cannot_be_indexed() {
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("tool-a-0.1.0.mur.zip", Vec::new(), "not a zip archive"),
            (
                "tool-a-0.2.0.mur.zip",
                payload("name: tool-a\nversion: 0.1.0\nruntime: tool\n", None),
                "does not match",
            ),
            (
                "tool-a-banana.mur.zip",
                payload("name: tool-a\nversion: banana\nruntime: tool\n", None),
                "not a semantic version",
            ),
            (
                "tool-a-latest.mur.zip",
                payload("name: tool-a\nversion: latest\nruntime: tool\n", None),
                "reserved",
            ),
            (
                "tool-a-0.1.0.mur.zip",
                payload(
                    "name: tool-a\nversion: 0.1.0\nruntime: tool\nimplementation: native\n",
                    None,
                ),
                "native payload names its platform",
            ),
            (
                "tool-a-0.1.0-linux-x86_64.mur.zip",
                payload("name: tool-a\nversion: 0.1.0\nruntime: tool\n", None),
                "runs on every platform",
            ),
            (
                "tool-a-0.1.0.mur.zip",
                labelled(
                    "name: tool-a\nversion: 0.1.0\nruntime: tool\n",
                    None,
                    Some("{\"built_at_level\": \"one\"}"),
                ),
                "its packed murmur-build.json sets built_at_level to \"one\"",
            ),
        ];
        for (file, bytes, reason) in cases {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(file), bytes).unwrap();
            let error = index_directory(dir.path(), "ana", "v1", &levels()).unwrap_err();
            let text = error.to_string();
            assert!(
                matches!(error, IndexBuildError::File { .. })
                    && text.contains(file)
                    && text.contains(reason),
                "{file}: {text}"
            );
        }
    }

    #[test]
    fn index_directory_refuses_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        assert!(matches!(
            index_directory(dir.path(), "ana", "v1", &levels()),
            Err(IndexBuildError::NoArtifacts { .. })
        ));
    }

    #[test]
    fn index_directory_writes_a_packed_label_and_derives_one_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let stale = importing(&["murmur:tool/run@0.0.9"]);
        // Packed labels are recorded as written, whatever the interfaces say.
        fs::write(
            dir.path().join("tool-a-0.2.0.mur.zip"),
            labelled(
                "name: tool-a\nversion: 0.2.0\nruntime: tool\n",
                Some(&stale),
                Some("{\"built_at_level\":1}\n"),
            ),
        )
        .unwrap();
        // No label, and interfaces fitting no level: no label at all.
        fs::write(
            dir.path().join("tool-a-0.1.0.mur.zip"),
            payload(
                "name: tool-a\nversion: 0.1.0\nruntime: tool\n",
                Some(&stale),
            ),
        )
        .unwrap();
        let built = index_directory(dir.path(), "ana", "v1", &levels()).unwrap();
        let versions = &built.artifacts[0].versions;
        assert_eq!(versions[0].built_at_level, Some(1));
        assert_eq!(versions[1].built_at_level, None);
        assert!(built.to_json().contains("\"built_at_level\": 1"));
    }

    #[test]
    fn index_directory_refuses_files_of_one_version_with_different_labels() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = "name: tool-n\nversion: 1.0.0\nruntime: tool\nimplementation: native\n";
        for (platform, level) in [("darwin-aarch64", 1), ("linux-x86_64", 2)] {
            fs::write(
                dir.path().join(format!("tool-n-1.0.0-{platform}.mur.zip")),
                labelled(
                    manifest,
                    None,
                    Some(&format!("{{\"built_at_level\":{level}}}")),
                ),
            )
            .unwrap();
        }
        let text = index_directory(dir.path(), "ana", "v1", &levels())
            .unwrap_err()
            .to_string();
        assert!(
            text.contains("tool-n-1.0.0-linux-x86_64.mur.zip")
                && text.contains("tool-n-1.0.0-darwin-aarch64.mur.zip")
                && text.contains("sets built_at_level 2")
                && text.contains("sets built_at_level 1"),
            "{text}"
        );
    }
}
