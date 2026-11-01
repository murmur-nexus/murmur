//! Where a scoped artifact comes from: each scope has exactly one source, a GitHub repository its
//! owner publishes from, and a source answers only for its own scope, through the
//! `artifacts-index.json` on its latest release.

pub(crate) mod github;
pub(crate) mod supply;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use capsule_runtime::served_wit_level;
use murmur_artifact::{
    artifact_name_format_error, load_manifest_from_artifact_bytes, sha256_hex, version_fit,
    ArtifactIndex, ArtifactIndexError, ArtifactName, FitSubject, IndexedArtifact, IndexedAsset,
    IndexedVersion, Manifest, MurmurLock, RegistryError, UnservedInterface, VersionFit,
    ARTIFACT_INDEX_FILE_NAME, MURMUR_SCOPE, RESERVED_SCOPES,
};

use crate::config::{
    mur_config_path, project_mur_config_path, read_mur_config_file, RegistryConfig, SourceConfig,
};
use crate::error::{
    local_store_miss, CliError, E_CFG_003, E_IDX_002, E_IO_003, E_REG_001, E_REG_002, E_REG_006,
    E_REG_007, E_REG_008, E_REG_009, E_REG_010, SCHEMA_1_INDEX_HINT,
};

use self::github::GitHubSource;

/// The repository Murmur's default artifacts are published from, the one source of the
/// [`MURMUR_SCOPE`].
pub(crate) const DEFAULT_ARTIFACTS_REPO: &str = "murmur-nexus/default-artifacts";

/// GitHub's numeric id of `murmur-nexus`, the account that owns [`DEFAULT_ARTIFACTS_REPO`]. The
/// [`MURMUR_SCOPE`] is held to it on every fetch, with or without a `murmur.lock`.
pub(crate) const MURMUR_OWNER_ID: u64 = 190_196_424;

/// A key read only to refuse it with `E-CFG-003`: a scope has one source, so there is no first
/// source to name.
pub(crate) const REMOVED_DEFAULT_KEY: &str = "registry.default";

/// The keys of the removed source form, which named a source and its type rather than its
/// repository.
const REMOVED_SOURCE_KEYS: [&str; 3] = ["name", "type", "url"];

/// Why a GitHub repository maps to no usable scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GithubScopeError {
    /// The lowercased owner is one of [`RESERVED_SCOPES`].
    Reserved { scope: String },
    /// The lowercased owner fails the part rule; `reason` is its wording.
    InvalidOwner { reason: String },
}

/// The scope every asset of `github:<owner>/<repo>` is named under: [`MURMUR_SCOPE`] for
/// [`DEFAULT_ARTIFACTS_REPO`], else the owner lowercased. Refused when the owner is no valid
/// scope, or names one of [`RESERVED_SCOPES`], which no GitHub owner can claim.
pub(crate) fn scope_of_github_repo(owner: &str, repo: &str) -> Result<String, GithubScopeError> {
    if format!("{owner}/{repo}").eq_ignore_ascii_case(DEFAULT_ARTIFACTS_REPO) {
        return Ok(MURMUR_SCOPE.to_string());
    }
    let scope = owner.to_ascii_lowercase();
    if RESERVED_SCOPES.contains(&scope.as_str()) {
        return Err(GithubScopeError::Reserved { scope });
    }
    if let Some(reason) = artifact_name_format_error(&scope) {
        return Err(GithubScopeError::InvalidOwner { reason });
    }
    Ok(scope)
}

/// The owner and repository of `repo`, written `<owner>/<repo>`: exactly two parts, neither empty
/// nor holding whitespace or a control character. Both go into request paths unencoded.
pub(crate) fn split_github_repo(repo: &str) -> Option<(&str, &str)> {
    let usable =
        |part: &str| !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c.is_control());
    repo.split_once('/')
        .filter(|(owner, name)| usable(owner) && usable(name) && !name.contains('/'))
}

/// The hint under a reserved-scope refusal of a GitHub owner.
pub(crate) fn reserved_scope_hint() -> String {
    format!(
        "'{}' names artifacts installed from a .mur.zip file and '{MURMUR_SCOPE}' names \
         those from {DEFAULT_ARTIFACTS_REPO}; no GitHub owner can claim either",
        murmur_artifact::LOCAL_SCOPE
    )
}

/// [`scope_of_github_repo`] for `mur install github:<owner>/<repo>`, refused `E-REG-007`.
pub(crate) fn github_scope(owner: &str, repo: &str) -> Result<String, CliError> {
    scope_of_github_repo(owner, repo).map_err(|error| match error {
        GithubScopeError::Reserved { scope } => CliError::with_hint(
            E_REG_007,
            format!("github:{owner}/{repo} cannot be installed: '{scope}' is a reserved scope"),
            reserved_scope_hint(),
        ),
        GithubScopeError::InvalidOwner { reason } => CliError::new(
            E_REG_007,
            format!(
                "github:{owner}/{repo} cannot be installed: the owner '{owner}' is not a valid \
                 artifact scope (it {reason})"
            ),
        ),
    })
}

// ─── configured sources ──────────────────────────────────────────────────────

/// The hint under every refusal of a source entry's shape.
const SOURCE_FORM_HINT: &str = "a source is one registry.sources entry, `- repo: <owner>/<repo>` \
     with an optional `token:`; it serves the scope named by its owner, and the murmur scope is \
     always served by murmur-nexus/default-artifacts";

/// One validated `registry.sources` entry.
#[derive(Debug, Clone)]
struct ConfiguredSource {
    scope: String,
    owner: String,
    repo: String,
    token: Option<String>,
}

/// The sources one config file's `registry:` section configures, or `E-CFG-003` naming `path`
/// and the offending key or entries.
fn configured_sources(
    path: &Path,
    registry: &RegistryConfig,
) -> Result<Vec<ConfiguredSource>, CliError> {
    let file = path.display();
    if registry.removed_default.is_some() {
        return Err(CliError::with_hint(
            E_CFG_003,
            format!(
                "{file} sets {REMOVED_DEFAULT_KEY}, which this mur no longer reads: each scope \
                 has exactly one source, so there is no first source to choose"
            ),
            format!("delete {REMOVED_DEFAULT_KEY}; {SOURCE_FORM_HINT}"),
        ));
    }

    let mut sources: Vec<(usize, ConfiguredSource)> = Vec::new();
    for (index, entry) in registry.sources.iter().enumerate() {
        let at = format!("registry.sources[{index}]");
        let source = configured_source(&file.to_string(), &at, entry)?;
        if let Some((earlier, other)) = sources
            .iter()
            .find(|(_, other)| other.scope == source.scope)
        {
            return Err(CliError::with_hint(
                E_CFG_003,
                format!(
                    "{file}: registry.sources[{earlier}] ({}/{}) and {at} ({}/{}) both serve \
                     scope '{}'",
                    other.owner, other.repo, source.owner, source.repo, source.scope
                ),
                "keep one of them: a scope has exactly one source",
            ));
        }
        sources.push((index, source));
    }
    Ok(sources.into_iter().map(|(_, source)| source).collect())
}

fn configured_source(
    file: &str,
    at: &str,
    entry: &SourceConfig,
) -> Result<ConfiguredSource, CliError> {
    let removed: Vec<&str> = REMOVED_SOURCE_KEYS
        .into_iter()
        .zip([&entry.name, &entry.removed_type, &entry.url])
        .filter(|(_, value)| value.is_some())
        .map(|(key, _)| key)
        .collect();
    if !removed.is_empty() {
        let keys = removed
            .iter()
            .map(|key| format!("{key}:"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::with_hint(
            E_CFG_003,
            format!("{file}: {at} sets {keys}, which this mur no longer reads"),
            SOURCE_FORM_HINT,
        ));
    }
    let Some(repo_ref) = entry
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
    else {
        return Err(CliError::with_hint(
            E_CFG_003,
            format!("{file}: {at} names no repo"),
            SOURCE_FORM_HINT,
        ));
    };
    let Some((owner, repo)) = split_github_repo(repo_ref) else {
        return Err(not_a_repository(file, at, repo_ref));
    };
    let scope = scope_of_github_repo(owner, repo).map_err(|error| match error {
        GithubScopeError::Reserved { scope } => CliError::with_hint(
            E_CFG_003,
            format!("{file}: {at} repo '{repo_ref}' claims the reserved scope '{scope}'"),
            reserved_scope_hint(),
        ),
        GithubScopeError::InvalidOwner { reason } => CliError::with_hint(
            E_CFG_003,
            format!(
                "{file}: {at} repo '{repo_ref}' names no usable scope: the owner '{owner}' is \
                 not a valid artifact scope (it {reason})"
            ),
            SOURCE_FORM_HINT,
        ),
    })?;
    Ok(ConfiguredSource {
        scope,
        owner: owner.to_string(),
        repo: repo.to_string(),
        token: entry.resolved_token(),
    })
}

fn not_a_repository(file: &str, at: &str, repo: &str) -> CliError {
    CliError::with_hint(
        E_CFG_003,
        format!("{file}: {at} repo '{repo}' is not a GitHub repository written <owner>/<repo>"),
        SOURCE_FORM_HINT,
    )
}

/// The source of every scope this command can fetch from: the built-in [`MURMUR_SCOPE`] source
/// and one per configured scope. Built without a request; each source contacts GitHub only when
/// first asked, and remembers what it was told for the rest of the command.
pub(crate) struct Sources {
    by_scope: BTreeMap<String, Arc<dyn ArtifactSource>>,
    /// The token configured for each `owner/repo`, lowercased, for a `github:` install.
    tokens: BTreeMap<String, Option<String>>,
}

impl Sources {
    /// The sources `mur install` uses: the global config file, then the project one, each
    /// checked on its own and merged by scope, the project's entry winning.
    pub(crate) fn load() -> Result<Self, CliError> {
        Self::from_files(&[mur_config_path()?, project_mur_config_path()?])
    }

    /// The sources `mur deploy` uses: the global config file only.
    #[cfg(any(feature = "beta-mur-deploy", test))]
    pub(crate) fn load_global() -> Result<Self, CliError> {
        Self::from_files(&[mur_config_path()?])
    }

    fn from_files(paths: &[PathBuf]) -> Result<Self, CliError> {
        let mut configured: BTreeMap<String, ConfiguredSource> = BTreeMap::new();
        for path in paths {
            let Some(config) = read_mur_config_file(path)? else {
                continue;
            };
            for source in configured_sources(path, &config.registry)? {
                configured.insert(source.scope.clone(), source);
            }
        }

        let builtin_token = configured
            .remove(MURMUR_SCOPE)
            .and_then(|source| source.token);
        let (owner, repo) = DEFAULT_ARTIFACTS_REPO
            .split_once('/')
            .expect("DEFAULT_ARTIFACTS_REPO is <owner>/<repo>");

        let mut tokens = BTreeMap::new();
        tokens.insert(DEFAULT_ARTIFACTS_REPO.to_string(), builtin_token.clone());
        let mut by_scope: BTreeMap<String, Arc<dyn ArtifactSource>> = BTreeMap::new();
        by_scope.insert(
            MURMUR_SCOPE.to_string(),
            Arc::new(GitHubSource::new(owner, repo, MURMUR_SCOPE, builtin_token)),
        );
        for (scope, source) in configured {
            tokens.insert(
                format!("{}/{}", source.owner, source.repo).to_ascii_lowercase(),
                source.token.clone(),
            );
            by_scope.insert(
                scope.clone(),
                Arc::new(GitHubSource::new(
                    &source.owner,
                    &source.repo,
                    &scope,
                    source.token,
                )),
            );
        }
        Ok(Self { by_scope, tokens })
    }

    /// The one source of `scope`, or `None` when no source serves it.
    pub(crate) fn for_scope(&self, scope: &str) -> Option<Arc<dyn ArtifactSource>> {
        self.by_scope.get(scope).cloned()
    }

    /// Every scope a source serves, sorted.
    pub(crate) fn scopes(&self) -> Vec<String> {
        self.by_scope.keys().cloned().collect()
    }

    /// The source of `name`'s scope. `local/` is refused as it always is, and a scope no source
    /// serves is refused `E-REG-008`; neither makes a request.
    pub(crate) fn source_for(
        &self,
        name: &ArtifactName,
        version: Option<&str>,
    ) -> Result<Arc<dyn ArtifactSource>, CliError> {
        if name.is_local() {
            return Err(local_store_miss(name, version));
        }
        self.for_scope(name.scope()).ok_or_else(|| {
            let scope = name.scope();
            CliError::with_hint(
                E_REG_008,
                format!(
                    "no source serves the '{scope}' scope, so {} cannot be looked up",
                    target(name, version)
                ),
                format!(
                    "add the GitHub repository '{scope}' publishes from to ~/.murmur/config.yaml \
                     or the project's .murmur/config.yaml:\n    registry:\n      sources:\n        \
                     - repo: {scope}/<repository>"
                ),
            )
        })
    }

    /// The token configured for `owner/repo`, if a `registry.sources` entry names it.
    pub(crate) fn token_for(&self, owner: &str, repo: &str) -> Option<String> {
        self.tokens
            .get(&format!("{owner}/{repo}").to_ascii_lowercase())
            .cloned()
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn from_sources(sources: Vec<Arc<dyn ArtifactSource>>) -> Self {
        Self {
            by_scope: sources
                .into_iter()
                .map(|source| (source.scope().to_string(), source))
                .collect(),
            tokens: BTreeMap::new(),
        }
    }
}

// ─── the source trait ────────────────────────────────────────────────────────

/// The `artifacts-index.json` a source publishes, and the release it was read from.
#[derive(Debug, Clone)]
pub(crate) struct PublishedIndex {
    pub release: String,
    pub index: ArtifactIndex,
}

/// One scope's source. Every method may be called from several threads at once, and answers each
/// question from the network at most once per command.
pub(crate) trait ArtifactSource: Send + Sync {
    /// `github:<owner>/<repo>`, how every message names the source.
    fn label(&self) -> &str;
    /// `<owner>/<repo>`.
    fn repo(&self) -> &str;
    /// The scope this source serves.
    fn scope(&self) -> &str;
    /// GitHub's numeric id of the repository's owner.
    fn owner_id(&self) -> Result<u64, SourceError>;
    /// The index on the repository's latest release.
    fn index(&self) -> Result<Arc<PublishedIndex>, SourceError>;
    /// The bytes of the asset `file` of release `release`.
    fn fetch(&self, release: &str, file: &str) -> Result<Bytes, SourceError>;
}

#[derive(Debug, Clone)]
pub(crate) enum SourceError {
    /// The source answered: it has no such repository, release or asset.
    NotFound(String),
    /// The source refused or failed the request for a reason other than a rate limit.
    Http { status: u16, message: String },
    /// The source refused the request because its API rate limit is exhausted.
    RateLimited {
        status: u16,
        message: String,
        /// The part of the response that identified the rate limit; printed in the reason.
        signal: RateLimitSignal,
        /// Seconds until the limit resets, or `None` when the response did not say.
        resets_in_secs: Option<u64>,
        /// Whether the refused request carried a token.
        authenticated: bool,
    },
    /// The request could not be made or its answer could not be read.
    Other(String),
    /// The repository has no release (`release` is `None`), or its latest release carries no
    /// `artifacts-index.json`.
    NoIndex { release: Option<String> },
    /// The latest release's `artifacts-index.json` is not a schema 2 index this mur reads.
    BadIndex {
        release: String,
        error: ArtifactIndexError,
    },
}

/// What identified a response as a rate limit, strongest signal first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateLimitSignal {
    /// Status 429.
    Status,
    /// `x-ratelimit-remaining: 0`.
    RemainingHeader,
    /// A `retry-after` header.
    RetryAfterHeader,
    /// Only the body said so. GitHub's wording is prose it may change, so this is the fallback.
    Body,
}

impl RateLimitSignal {
    /// The evidence printed after the status, or `None` when the status alone is the signal.
    fn evidence(self) -> Option<&'static str> {
        match self {
            RateLimitSignal::Status => None,
            RateLimitSignal::RemainingHeader => Some("x-ratelimit-remaining: 0"),
            RateLimitSignal::RetryAfterHeader => Some("retry-after header"),
            RateLimitSignal::Body => Some("response body says rate limit"),
        }
    }
}

impl SourceError {
    /// One line saying what the source answered instead.
    pub(crate) fn reason(&self) -> String {
        match self {
            SourceError::NotFound(message) | SourceError::Other(message) => message.clone(),
            SourceError::Http { status, message } => format!("HTTP {status}: {message}"),
            SourceError::RateLimited {
                status,
                message,
                signal,
                resets_in_secs,
                ..
            } => {
                let mut reason = match signal.evidence() {
                    Some(evidence) => {
                        format!("rate limited by GitHub (HTTP {status}, {evidence}): {message}")
                    }
                    None => format!("rate limited by GitHub (HTTP {status}): {message}"),
                };
                if let Some(secs) = resets_in_secs {
                    let minutes = secs.div_ceil(60).max(1);
                    let unit = if minutes == 1 { "minute" } else { "minutes" };
                    reason.push_str(&format!(" — resets in about {minutes} {unit}"));
                }
                reason
            }
            SourceError::NoIndex { release: None } => "no published release".to_string(),
            SourceError::NoIndex {
                release: Some(release),
            } => format!("no {ARTIFACT_INDEX_FILE_NAME} on release {release}"),
            SourceError::BadIndex { release, error } => {
                format!("{ARTIFACT_INDEX_FILE_NAME} on release {release} {error}")
            }
        }
    }
}

// ─── resolution ──────────────────────────────────────────────────────────────

/// A payload [`resolve`] fetched and checked against the source's index.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub bytes: Bytes,
    /// The digest the index lists, which the bytes were checked against.
    pub sha256: String,
    pub version: String,
    /// The platform tag of the asset chosen, or `None` for an untagged one.
    pub platform: Option<String>,
    /// [`ArtifactSource::label`] of the source that served it.
    pub source: String,
    /// The owner id the source answered with, already checked.
    pub owner_id: u64,
    /// The payload's packed `murmur.yaml`, whose `name:` and `version:` were checked.
    pub manifest: Manifest,
}

/// Where the owner id a scope is held to was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordedOwner {
    /// Compiled in, for [`MURMUR_SCOPE`]: [`MURMUR_OWNER_ID`].
    BuiltIn,
    /// On the `murmur.lock` entry `entry`.
    Lock { entry: String, owner_id: u64 },
}

/// Why [`resolve`] fetched nothing, or nothing it could hand back.
#[derive(Debug, Clone)]
pub(crate) enum ResolveError {
    /// The source did not answer a request.
    Unanswered {
        source: String,
        target: String,
        error: SourceError,
    },
    /// The repository does not exist, or is not visible with the credentials sent.
    RepositoryMissing { source: String, target: String },
    /// The index lists no artifact of this name.
    NameNotListed {
        source: String,
        release: String,
        target: String,
        name: ArtifactName,
    },
    /// The index lists the artifact, not at this version.
    VersionNotListed {
        source: String,
        name: ArtifactName,
        version: String,
        listed: Vec<String>,
    },
    /// The version has no asset this platform runs.
    NoPayloadFor {
        source: String,
        name: ArtifactName,
        version: String,
        platform: String,
        published: Vec<String>,
    },
    /// An explicitly named version does not fit the WIT level this mur serves. `fitting` is the
    /// newest version the same index lists that does.
    DoesNotFit {
        source: String,
        name: ArtifactName,
        version: String,
        mismatched: Vec<UnservedInterface>,
        fitting: Option<String>,
    },
    /// No version of an unversioned reference fits this mur; `newest` is the newest listed.
    NoneFits {
        source: String,
        name: ArtifactName,
        newest: String,
        fit: VersionFit,
    },
    /// The index names an asset its release does not hold.
    AssetMissing {
        source: String,
        target: String,
        release: String,
        file: String,
        reason: String,
    },
    /// No index on the latest release (or no release at all).
    NoIndex {
        source: String,
        repo: String,
        target: String,
        release: Option<String>,
    },
    /// The index is not a schema 2 index this mur reads.
    BadIndex {
        source: String,
        repo: String,
        target: String,
        release: String,
        error: ArtifactIndexError,
    },
    /// The index names a scope the source does not own.
    ScopeClaimed {
        source: String,
        target: String,
        claimed: String,
        scope: String,
    },
    /// The scope's GitHub owner is not the one recorded for it. `target` is what was being
    /// resolved when the owner was asked.
    OwnerChanged {
        source: String,
        target: String,
        scope: String,
        current: u64,
        recorded: RecordedOwner,
    },
    /// The payload's digest is not the one the index lists.
    Digest {
        source: String,
        target: String,
        file: String,
        expected: String,
        actual: String,
    },
    /// The payload packs another `version:` than the index lists it under.
    PackedVersion {
        source: String,
        file: String,
        name: ArtifactName,
        version: String,
        packed: String,
    },
    /// The payload packs another `name:` than was asked for.
    PackedName {
        name: ArtifactName,
        version: String,
        packed: String,
    },
    /// The payload is not a readable `.mur.zip`.
    Unreadable {
        source: String,
        target: String,
        file: String,
        error: String,
    },
}

/// Whether [`resolve`] holds an explicitly named version to the WIT level this mur serves. A
/// version left out is always the newest that fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplicitVersion {
    /// Refused with [`ResolveError::DoesNotFit`] before anything is fetched: an install on this
    /// host.
    FitChecked,
    /// Fetched as asked: a deploy, for a target `mur` whose level this one cannot know.
    #[cfg(feature = "beta-mur-deploy")]
    AsAsked,
}

/// `<scope>/<name>@<version>`, or `<scope>/<name>` for an unversioned reference.
fn target(name: &ArtifactName, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{name}@{version}"),
        None => name.to_string(),
    }
}

/// Fetch `name` from `source`, which must be the source of its scope: `version` exactly, or with
/// `None` the newest version that fits this mur, as the asset for `platform` (tagged for it, else
/// untagged). Nothing is written: the caller stores what this returns.
///
/// Checked in order, each before anything further is requested: the owner id against `lock` and,
/// for the [`MURMUR_SCOPE`], against [`MURMUR_OWNER_ID`]; the index's scope against the
/// source's; under [`ExplicitVersion::FitChecked`], an explicit `version`'s interfaces against the
/// served WIT level; then, once the payload is fetched, its SHA-256 against the index and its
/// packed `name:` and `version:` against what was asked.
pub(crate) fn resolve(
    source: &dyn ArtifactSource,
    name: &ArtifactName,
    version: Option<&str>,
    explicit: ExplicitVersion,
    platform: &str,
    lock: Option<&MurmurLock>,
) -> Result<Resolved, Box<ResolveError>> {
    let label = source.label().to_string();
    let wanted = target(name, version);
    let unanswered = |error: SourceError| {
        Box::new(match error {
            SourceError::NotFound(_) => ResolveError::RepositoryMissing {
                source: label.clone(),
                target: wanted.clone(),
            },
            SourceError::NoIndex { release } => ResolveError::NoIndex {
                source: label.clone(),
                repo: source.repo().to_string(),
                target: wanted.clone(),
                release,
            },
            SourceError::BadIndex { release, error } => ResolveError::BadIndex {
                source: label.clone(),
                repo: source.repo().to_string(),
                target: wanted.clone(),
                release,
                error,
            },
            error => ResolveError::Unanswered {
                source: label.clone(),
                target: wanted.clone(),
                error,
            },
        })
    };

    let owner_id = source.owner_id().map_err(unanswered)?;
    check_owner(source, owner_id, lock, &wanted)?;

    let published = source.index().map_err(unanswered)?;
    if published.index.scope != source.scope() {
        return Err(Box::new(ResolveError::ScopeClaimed {
            source: label,
            target: wanted,
            claimed: published.index.scope.clone(),
            scope: source.scope().to_string(),
        }));
    }

    let artifact = published
        .index
        .artifacts
        .iter()
        .find(|artifact| artifact.name == name.name())
        .ok_or_else(|| {
            Box::new(ResolveError::NameNotListed {
                source: label.clone(),
                release: published.release.clone(),
                target: wanted.clone(),
                name: name.clone(),
            })
        })?;

    let chosen = match version {
        Some(version) => select_version(artifact, version).ok_or_else(|| {
            Box::new(ResolveError::VersionNotListed {
                source: label.clone(),
                name: name.clone(),
                version: version.to_string(),
                listed: artifact
                    .versions
                    .iter()
                    .map(|v| v.version.clone())
                    .collect(),
            })
        })?,
        None => newest_fitting(artifact, platform).ok_or_else(|| {
            let newest = &artifact.versions[0];
            Box::new(ResolveError::NoneFits {
                source: label.clone(),
                name: name.clone(),
                newest: newest.version.clone(),
                fit: index_fit(newest, platform),
            })
        })?,
    };

    if version.is_some() && explicit == ExplicitVersion::FitChecked {
        if let VersionFit::Unserved(mismatched) = index_fit(chosen, platform) {
            return Err(Box::new(ResolveError::DoesNotFit {
                source: label,
                name: name.clone(),
                version: chosen.version.clone(),
                mismatched,
                fitting: newest_fitting(artifact, platform).map(|fits| fits.version.clone()),
            }));
        }
    }

    let asset = select_asset(chosen, platform).ok_or_else(|| {
        Box::new(ResolveError::NoPayloadFor {
            source: label.clone(),
            name: name.clone(),
            version: chosen.version.clone(),
            platform: platform.to_string(),
            published: chosen
                .assets
                .iter()
                .filter_map(|asset| asset.platform.clone())
                .collect(),
        })
    })?;

    let bytes = source
        .fetch(&chosen.release, &asset.file)
        .map_err(|error| match error {
            SourceError::NotFound(reason) => Box::new(ResolveError::AssetMissing {
                source: label.clone(),
                target: format!("{name}@{}", chosen.version),
                release: chosen.release.clone(),
                file: asset.file.clone(),
                reason,
            }),
            error => unanswered(error),
        })?;

    let actual = sha256_hex(&bytes);
    if actual != asset.sha256 {
        return Err(Box::new(ResolveError::Digest {
            source: label,
            target: wanted,
            file: asset.file.clone(),
            expected: asset.sha256.clone(),
            actual,
        }));
    }

    let manifest = load_manifest_from_artifact_bytes(&bytes).map_err(|error| {
        Box::new(ResolveError::Unreadable {
            source: label.clone(),
            target: wanted.clone(),
            file: asset.file.clone(),
            error: error.to_string(),
        })
    })?;
    if manifest.name != name.name() {
        return Err(Box::new(ResolveError::PackedName {
            name: name.clone(),
            version: chosen.version.clone(),
            packed: manifest.name,
        }));
    }
    if manifest.version.trim() != chosen.version {
        return Err(Box::new(ResolveError::PackedVersion {
            source: label,
            file: asset.file.clone(),
            name: name.clone(),
            version: chosen.version.clone(),
            packed: manifest.version,
        }));
    }

    Ok(Resolved {
        bytes,
        sha256: actual,
        version: chosen.version.clone(),
        platform: asset.platform.clone(),
        source: label,
        owner_id,
        manifest,
    })
}

/// The owner id `scope` is held to: [`MURMUR_OWNER_ID`] for the [`MURMUR_SCOPE`], else an id
/// `lock` records for one of the scope's entries that differs from `owner_id`.
fn recorded_conflict(
    scope: &str,
    owner_id: u64,
    lock: Option<&MurmurLock>,
) -> Option<RecordedOwner> {
    if scope == MURMUR_SCOPE && owner_id != MURMUR_OWNER_ID {
        return Some(RecordedOwner::BuiltIn);
    }
    let entry = lock?.owner_conflict(scope, owner_id)?;
    Some(RecordedOwner::Lock {
        entry: entry.name.clone(),
        owner_id: entry.owner_id?,
    })
}

/// `E-REG-010` unless `owner_id`, what `source` answered for its repository's owner, is the owner
/// its scope is held to. `target` names what is being resolved, for the message.
pub(crate) fn check_owner(
    source: &dyn ArtifactSource,
    owner_id: u64,
    lock: Option<&MurmurLock>,
    target: &str,
) -> Result<(), Box<ResolveError>> {
    match recorded_conflict(source.scope(), owner_id, lock) {
        Some(recorded) => Err(Box::new(ResolveError::OwnerChanged {
            source: source.label().to_string(),
            target: target.to_string(),
            scope: source.scope().to_string(),
            current: owner_id,
            recorded,
        })),
        None => Ok(()),
    }
}

/// `E-REG-010` when a stored payload of `name@version` records an owner id its scope is not held
/// to. `store` names the store in the message. Makes no request.
pub(crate) fn check_stored_owner(
    name: &ArtifactName,
    version: &str,
    store: &str,
    owner_id: Option<u64>,
    lock: Option<&MurmurLock>,
) -> Result<(), CliError> {
    let Some(owner_id) = owner_id else {
        return Ok(());
    };
    let Some(recorded) = recorded_conflict(name.scope(), owner_id, lock) else {
        return Ok(());
    };
    let scope = name.scope();
    let held_to = match &recorded {
        RecordedOwner::BuiltIn => format!(
            "scope '{scope}' is owned by GitHub account id {MURMUR_OWNER_ID}, the owner of \
             {DEFAULT_ARTIFACTS_REPO}"
        ),
        RecordedOwner::Lock { entry, owner_id } => {
            format!("murmur.lock records owner id {owner_id} for scope '{scope}' (entry {entry})")
        }
    };
    Err(CliError::with_hint(
        E_REG_010,
        format!(
            "{name}@{version} in {store} was installed from GitHub account id {owner_id}, and \
             {held_to}"
        ),
        format!(
            "the stored payload came from another owner of '{scope}': its account was renamed \
             and the name registered again, or the repository changed hands; if you trust the new \
             owner, delete the '{scope}' entries from murmur.lock and install again, otherwise \
             remove {name}@{version} from {store}"
        ),
    ))
}

/// The version of `artifact` the index lists as exactly `version`; `v1.0.0` is not `1.0.0`.
pub(crate) fn select_version<'a>(
    artifact: &'a IndexedArtifact,
    version: &str,
) -> Option<&'a IndexedVersion> {
    artifact.versions.iter().find(|v| v.version == version)
}

/// The newest version of `artifact` that fits this mur on `platform`: it fits the WIT level this
/// mur serves, and one of its payloads runs on `platform`. The same predicate `mur search` lists
/// by, so both name the same version.
pub(crate) fn newest_fitting<'a>(
    artifact: &'a IndexedArtifact,
    platform: &str,
) -> Option<&'a IndexedVersion> {
    artifact.newest_fitting(served_wit_level(), platform)
}

/// Whether `version` fits this mur on `platform`, and why not when it does not: the predicate
/// [`newest_fitting`] applies to each version.
pub(crate) fn index_fit(version: &IndexedVersion, platform: &str) -> VersionFit {
    version_fit(&FitSubject::indexed(version), served_wit_level(), platform)
}

/// The asset of `version` that `platform` runs: the one tagged for it, else the untagged one,
/// never another platform's.
pub(crate) fn select_asset<'a>(
    version: &'a IndexedVersion,
    platform: &str,
) -> Option<&'a IndexedAsset> {
    version
        .assets
        .iter()
        .find(|asset| asset.platform.as_deref() == Some(platform))
        .or_else(|| version.assets.iter().find(|asset| asset.platform.is_none()))
}

// ─── rendering ───────────────────────────────────────────────────────────────

const ANONYMOUS_RATE_LIMIT_HINT: &str = concat!(
    "set GITHUB_TOKEN to any GitHub token, for example `export GITHUB_TOKEN=$(gh auth token)`, or \
     point the source's `token:` in config.yaml at a variable you already export, such as `token: \
     ${GH_TOKEN}` — GitHub allows 60 unauthenticated API requests an hour and the first artifact \
     of a scope can spend five; see ",
    murmur_artifact::docs_reference_url!("installing-artifacts/")
);

const TOKEN_RATE_LIMIT_HINT: &str = "the token sent with these requests has exhausted its own \
     GitHub rate limit; retry after the reset shown above";

const UNANSWERED_HINT: &str = "the source's answer is shown above; fix the source or its \
     credentials and retry";

/// The page listing which `mur` release serves which WIT level.
const WIT_LEVELS_PAGE: &str = murmur_artifact::docs_reference_url!("wit-levels/");

const OWNER_CHANGED_HINT: &str = "the GitHub account that owned the scope was renamed and its \
     name registered again by someone else, or the repository changed hands";

/// The hint for an unanswered request.
pub(crate) fn unanswered_hint(error: &SourceError) -> &'static str {
    match error {
        SourceError::RateLimited {
            authenticated: false,
            ..
        } => ANONYMOUS_RATE_LIMIT_HINT,
        SourceError::RateLimited {
            authenticated: true,
            ..
        } => TOKEN_RATE_LIMIT_HINT,
        _ => UNANSWERED_HINT,
    }
}

/// The note `mur search` prints for a version that does not fit: why `full_name@version` does not
/// load on this host, or `None` when it fits.
pub(crate) fn fit_note(full_name: &str, version: &str, fit: &VersionFit) -> Option<String> {
    match fit {
        VersionFit::Fits => None,
        VersionFit::Unserved(unserved) => {
            let first = &unserved[0];
            let more = if unserved.len() > 1 {
                format!(" (+{} more)", unserved.len() - 1)
            } else {
                String::new()
            };
            Some(format!(
                "{full_name}: {version} needs {}, {}{more}",
                crate::stores::interface_label(first),
                crate::stores::served_phrase(first)
            ))
        }
        VersionFit::NoPayloadFor(platform) => Some(format!(
            "{full_name}: {version} has no payload for {platform}"
        )),
    }
}

fn index_hint(repo: &str, release: Option<&str>) -> String {
    let release = release.unwrap_or("<tag>");
    format!(
        "its publisher writes the index with `mur index <dir> --repo {repo} --release {release}` \
         and uploads {ARTIFACT_INDEX_FILE_NAME} to release {release}"
    )
}

impl From<ResolveError> for CliError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::Unanswered {
                source,
                target,
                error,
            } => CliError::with_hint(
                E_REG_006,
                format!(
                    "could not look up {target}: {source} did not answer — {}",
                    error.reason()
                ),
                unanswered_hint(&error),
            ),
            ResolveError::RepositoryMissing { source, target } => CliError::with_hint(
                E_REG_001,
                format!(
                    "could not resolve {target}: {source} has no such repository, or it is not \
                     visible with the credentials sent"
                ),
                "check the repo of the scope's registry.sources entry; a private repository \
                 needs a `token:`",
            ),
            ResolveError::NameNotListed {
                source,
                release,
                target,
                name,
            } => CliError::with_hint(
                E_REG_001,
                format!(
                    "could not resolve {target}: the index of {source} (release {release}) lists \
                     no {name}"
                ),
                "check the name: it must be one the scope's index lists exactly",
            ),
            ResolveError::VersionNotListed {
                source,
                name,
                version,
                listed,
            } => CliError::with_hint(
                E_REG_001,
                format!(
                    "could not resolve {name}@{version}: the index of {source} lists {name} at \
                     {}",
                    listed.join(", ")
                ),
                format!(
                    "name one of those versions, or omit the version to install the newest that \
                     fits this mur: mur install {name}"
                ),
            ),
            ResolveError::NoPayloadFor {
                source,
                name,
                version,
                platform,
                published,
            } => {
                let publishes = if published.is_empty() {
                    "it publishes no payload tagged for any platform".to_string()
                } else {
                    format!("it publishes {}", published.join(", "))
                };
                CliError::with_hint(
                    E_REG_001,
                    format!(
                        "could not resolve {name}@{version}: {source} has no payload of it for \
                         {platform}; {publishes}"
                    ),
                    format!(
                        "build it on this host with `mur build` then `mur install <path>`, or \
                         ask the publisher to release a {platform} asset"
                    ),
                )
            }
            ResolveError::NoneFits {
                source,
                name,
                newest,
                fit,
            } => {
                let note = fit_note(name.as_str(), &newest, &fit).unwrap_or_default();
                let level = served_wit_level().level;
                CliError::with_hint(
                    E_REG_001,
                    format!(
                        "no version of {name} in {source} fits WIT level {level}, the level this \
                         mur serves; the newest is {note}"
                    ),
                    match fit {
                        VersionFit::NoPayloadFor(platform) => format!(
                            "build it on this host with `mur build` then `mur install <path>`, or \
                             ask its publisher to release a {platform} asset"
                        ),
                        VersionFit::Fits | VersionFit::Unserved(_) => format!(
                            "{WIT_LEVELS_PAGE} lists which mur release serves which WIT level; \
                             run one that serves the level {name} was built at, or ask its \
                             publisher to rebuild it against level {level}"
                        ),
                    },
                )
            }
            ResolveError::DoesNotFit {
                source,
                name,
                version,
                mismatched,
                fitting,
            } => crate::wit_level::index_level_mismatch(
                name.as_str(),
                &version,
                mismatched,
                &source,
                fitting.as_deref(),
            ),
            ResolveError::AssetMissing {
                source,
                target,
                release,
                file,
                reason,
            } => CliError::with_hint(
                E_REG_001,
                format!(
                    "could not resolve {target}: the index of {source} lists it as {file} on \
                     release {release}, and {reason}"
                ),
                "the source's index and its releases disagree; its publisher regenerates the \
                 index with `mur index`",
            ),
            ResolveError::NoIndex {
                source,
                repo,
                target,
                release,
            } => {
                let message = match &release {
                    Some(release) => format!(
                        "{source} publishes no {ARTIFACT_INDEX_FILE_NAME} on its latest release \
                         {release}, so {target} cannot be looked up"
                    ),
                    None => format!(
                        "{source} has no published release, so it publishes no \
                         {ARTIFACT_INDEX_FILE_NAME} and {target} cannot be looked up"
                    ),
                };
                CliError::with_hint(E_IDX_002, message, index_hint(&repo, release.as_deref()))
            }
            ResolveError::BadIndex {
                source,
                repo,
                target,
                release,
                error,
            } => {
                let hint = match error {
                    ArtifactIndexError::Schema1 => SCHEMA_1_INDEX_HINT.to_string(),
                    _ => index_hint(&repo, Some(&release)),
                };
                CliError::with_hint(
                    E_IDX_002,
                    format!(
                        "could not resolve {target}: artifact index {ARTIFACT_INDEX_FILE_NAME} of \
                         {source} release {release} {error}"
                    ),
                    hint,
                )
            }
            ResolveError::ScopeClaimed {
                source,
                target,
                claimed,
                scope,
            } => {
                let message = if claimed == MURMUR_SCOPE {
                    format!(
                        "could not resolve {target}: {source} publishes an index for scope \
                         '{claimed}', which only {DEFAULT_ARTIFACTS_REPO} serves; this source \
                         serves scope '{scope}'"
                    )
                } else {
                    format!(
                        "could not resolve {target}: {source} publishes an index for scope \
                         '{claimed}', and it serves scope '{scope}'"
                    )
                };
                CliError::with_hint(
                    E_REG_009,
                    message,
                    format!(
                        "a source answers only for the scope of its owner, so nothing was \
                         installed; its publisher regenerates the index with `mur index <dir> \
                         --repo {} --release <tag>`",
                        source.trim_start_matches("github:")
                    ),
                )
            }
            ResolveError::OwnerChanged {
                source,
                target,
                scope,
                current,
                recorded,
            } => match recorded {
                RecordedOwner::BuiltIn => CliError::with_hint(
                    E_REG_010,
                    format!(
                        "could not resolve {target}: scope '{scope}' is owned by GitHub account id \
                         {MURMUR_OWNER_ID}, the owner of {DEFAULT_ARTIFACTS_REPO}, and {source} \
                         answered with owner id {current}"
                    ),
                    "that owner id is compiled into mur, so no other account can serve the \
                     murmur scope; check that MUR_GITHUB_API_BASE is unset and that \
                     api.github.com is reached directly",
                ),
                RecordedOwner::Lock { entry, owner_id } => CliError::with_hint(
                    E_REG_010,
                    format!(
                        "could not resolve {target}: the owner of scope '{scope}' has changed: \
                         {source} is now owned by GitHub account id {current}, and murmur.lock \
                         records owner id {owner_id} (entry {entry})"
                    ),
                    format!(
                        "{OWNER_CHANGED_HINT}; if you trust the new owner, delete the \
                         '{scope}' entries from murmur.lock and install again"
                    ),
                ),
            },
            ResolveError::Digest {
                source,
                target,
                file,
                expected,
                actual,
            } => CliError::with_hint(
                E_REG_002,
                format!(
                    "could not resolve {target}: {file} from {source} has sha256 {actual}, and \
                     its index lists sha256 {expected}"
                ),
                "the payload is not the one the index describes, so nothing was stored; its \
                 publisher regenerates the index, or the download was altered on the way",
            ),
            ResolveError::PackedVersion {
                source,
                file,
                name,
                version,
                packed,
            } => CliError::with_hint(
                E_REG_002,
                format!(
                    "{file} from {source} packs version '{packed}', and its index lists it as \
                     {name}@{version}"
                ),
                "the asset was built before its version was set, so nothing was stored; its \
                 publisher rebuilds it and regenerates the index",
            ),
            ResolveError::PackedName {
                name,
                version,
                packed,
            } => CliError::from(RegistryError::PackedNameMismatch {
                name: name.to_string(),
                version,
                packed,
            }),
            ResolveError::Unreadable {
                source,
                target,
                file,
                error,
            } => CliError::new(
                E_IO_003,
                format!(
                    "could not resolve {target}: {file} from {source} is not a valid .mur.zip \
                     artifact: {error}"
                ),
            ),
        }
    }
}

impl From<Box<ResolveError>> for CliError {
    fn from(error: Box<ResolveError>) -> Self {
        CliError::from(*error)
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error = CliError::from(self.clone());
        write!(f, "{}", error.message)?;
        if let Some(hint) = error.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

/// An in-memory [`ArtifactSource`] for unit tests: an index built from what is published to it,
/// and a record of every fetch.
#[cfg(test)]
pub(crate) mod memory {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use murmur_artifact::{
        sha256_hex, ArtifactIndex, IndexedArtifact, IndexedAsset, IndexedVersion,
    };

    use super::{ArtifactSource, PublishedIndex, SourceError};

    pub(crate) struct MemorySource {
        label: String,
        repo: String,
        scope: String,
        pub owner_id: u64,
        pub index: ArtifactIndex,
        pub release: String,
        files: BTreeMap<(String, String), Bytes>,
        /// Overrides [`ArtifactSource::index`] when set.
        pub index_error: Option<SourceError>,
        /// Every `(release, file)` fetched, in order.
        pub fetches: Mutex<Vec<(String, String)>>,
    }

    impl MemorySource {
        /// A source for `owner/repo` serving `scope`, whose latest release is `release`.
        pub(crate) fn new(repo: &str, scope: &str, owner_id: u64, release: &str) -> Self {
            Self {
                label: format!("github:{repo}"),
                repo: repo.to_string(),
                scope: scope.to_string(),
                owner_id,
                index: ArtifactIndex {
                    schema_version: 2,
                    scope: scope.to_string(),
                    artifacts: Vec::new(),
                },
                release: release.to_string(),
                files: BTreeMap::new(),
                index_error: None,
                fetches: Mutex::new(Vec::new()),
            }
        }

        /// Publish `bytes` as `name@version` on `release`, for `platform` or untagged, listing
        /// `interfaces`. Versions of one name are kept newest first in publishing order, so
        /// publish older versions first.
        pub(crate) fn publish(
            &mut self,
            release: &str,
            name: &str,
            version: &str,
            platform: Option<&str>,
            interfaces: &[&str],
            bytes: Vec<u8>,
        ) -> &mut Self {
            let file = murmur_artifact::asset_file_name(name, version, platform);
            let asset = IndexedAsset {
                file: file.clone(),
                platform: platform.map(str::to_string),
                sha256: sha256_hex(&bytes),
            };
            self.files
                .insert((release.to_string(), file), Bytes::from(bytes));
            let artifact = match self.index.artifacts.iter_mut().find(|a| a.name == name) {
                Some(artifact) => artifact,
                None => {
                    self.index.artifacts.push(IndexedArtifact {
                        name: name.to_string(),
                        runtime: "tool".to_string(),
                        description: None,
                        tags: Vec::new(),
                        versions: Vec::new(),
                    });
                    self.index.artifacts.last_mut().unwrap()
                }
            };
            match artifact.versions.iter_mut().find(|v| v.version == version) {
                Some(existing) => existing.assets.push(asset),
                None => artifact.versions.insert(
                    0,
                    IndexedVersion {
                        version: version.to_string(),
                        release: release.to_string(),
                        built_at_level: None,
                        interfaces: interfaces.iter().map(|i| i.to_string()).collect(),
                        assets: vec![asset],
                    },
                ),
            }
            self
        }

        pub(crate) fn fetched(&self) -> Vec<(String, String)> {
            self.fetches.lock().unwrap().clone()
        }
    }

    impl ArtifactSource for MemorySource {
        fn label(&self) -> &str {
            &self.label
        }

        fn repo(&self) -> &str {
            &self.repo
        }

        fn scope(&self) -> &str {
            &self.scope
        }

        fn owner_id(&self) -> Result<u64, SourceError> {
            Ok(self.owner_id)
        }

        fn index(&self) -> Result<Arc<PublishedIndex>, SourceError> {
            if let Some(error) = &self.index_error {
                return Err(error.clone());
            }
            Ok(Arc::new(PublishedIndex {
                release: self.release.clone(),
                index: self.index.clone(),
            }))
        }

        fn fetch(&self, release: &str, file: &str) -> Result<Bytes, SourceError> {
            self.fetches
                .lock()
                .unwrap()
                .push((release.to_string(), file.to_string()));
            self.files
                .get(&(release.to_string(), file.to_string()))
                .cloned()
                .ok_or_else(|| SourceError::NotFound(format!("release {release} has no {file}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use murmur_artifact::{current_platform, LockOrigin, LockedSha256, LOCK_VERSION};
    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::memory::MemorySource;
    use super::*;

    fn zip_bytes(name: &str, version: &str) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let options = SimpleFileOptions::default();
            zip.start_file("murmur.yaml", options).unwrap();
            write!(zip, "name: {name}\nversion: {version}\nruntime: tool\n").unwrap();
            zip.start_file("tool.wasm", options).unwrap();
            zip.write_all(b"\0asm\x01\0\0\0").unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// The `murmur:tool/run` name this mur serves.
    fn served_tool_run() -> String {
        let version = served_wit_level()
            .version_of("murmur:tool")
            .expect("this mur serves murmur:tool");
        format!("murmur:tool/run@{version}")
    }

    fn other_platform() -> &'static str {
        murmur_artifact::SUPPORTED_PLATFORMS
            .iter()
            .copied()
            .find(|platform| *platform != current_platform())
            .unwrap()
    }

    fn ana() -> MemorySource {
        let mut source = MemorySource::new("ana/artifacts", "ana", 1111, "v2");
        source
            .publish(
                "v1",
                "tool-git",
                "0.9.0",
                None,
                &[],
                zip_bytes("tool-git", "0.9.0"),
            )
            .publish(
                "v2",
                "tool-git",
                "1.0.0",
                None,
                &[],
                zip_bytes("tool-git", "1.0.0"),
            )
            .publish(
                "v2",
                "tool-git-validate",
                "1.0.0",
                None,
                &[],
                zip_bytes("tool-git-validate", "1.0.0"),
            );
        source
    }

    fn name(full: &str) -> ArtifactName {
        ArtifactName::parse(full).unwrap()
    }

    fn resolve_in(
        source: &MemorySource,
        full: &str,
        version: Option<&str>,
    ) -> Result<Resolved, Box<ResolveError>> {
        resolve(
            source,
            &name(full),
            version,
            ExplicitVersion::FitChecked,
            current_platform(),
            None,
        )
    }

    #[test]
    fn a_name_resolves_to_its_own_file_and_never_to_a_longer_one() {
        let source = ana();
        let resolved = resolve_in(&source, "ana/tool-git", Some("1.0.0")).unwrap();
        assert_eq!(resolved.manifest.name, "tool-git");
        assert_eq!(resolved.version, "1.0.0");
        assert_eq!(resolved.owner_id, 1111);
        assert_eq!(resolved.source, "github:ana/artifacts");
        assert_eq!(
            source.fetched(),
            [("v2".to_string(), "tool-git-1.0.0.mur.zip".to_string())]
        );

        let error = resolve_in(&source, "ana/tool-gi", Some("1.0.0")).unwrap_err();
        assert!(
            matches!(&*error, ResolveError::NameNotListed { name, .. } if name.as_str() == "ana/tool-gi"),
            "{error:?}"
        );
    }

    #[test]
    fn a_version_matches_exactly() {
        let source = ana();
        let artifact = &source.index.artifacts[0];
        assert_eq!(select_version(artifact, "0.9.0").unwrap().release, "v1");
        assert!(select_version(artifact, "v1.0.0").is_none());
        assert!(select_version(artifact, "1.0").is_none());

        let error = resolve_in(&source, "ana/tool-git", Some("2.0.0")).unwrap_err();
        let cli = CliError::from(error);
        assert_eq!(cli.code, E_REG_001);
        assert!(
            cli.message.contains("ana/tool-git@2.0.0")
                && cli.message.contains("github:ana/artifacts")
                && cli.message.contains("1.0.0, 0.9.0"),
            "{}",
            cli.message
        );
    }

    fn tagged_version(assets: &[(Option<&str>, &str)]) -> IndexedVersion {
        IndexedVersion {
            version: "1.0.0".to_string(),
            release: "v1".to_string(),
            built_at_level: None,
            interfaces: Vec::new(),
            assets: assets
                .iter()
                .map(|(platform, file)| IndexedAsset {
                    file: file.to_string(),
                    platform: platform.map(str::to_string),
                    sha256: "0".repeat(64),
                })
                .collect(),
        }
    }

    #[test]
    fn this_platforms_tagged_asset_is_chosen_over_the_untagged_one() {
        let version = tagged_version(&[
            (Some("darwin-aarch64"), "t-1.0.0-darwin-aarch64.mur.zip"),
            (None, "t-1.0.0.mur.zip"),
            (Some("linux-x86_64"), "t-1.0.0-linux-x86_64.mur.zip"),
        ]);
        assert_eq!(
            select_asset(&version, "linux-x86_64").unwrap().file,
            "t-1.0.0-linux-x86_64.mur.zip"
        );
        assert_eq!(
            select_asset(&version, "linux-aarch64").unwrap().file,
            "t-1.0.0.mur.zip"
        );
    }

    #[test]
    fn another_platforms_asset_is_never_chosen() {
        let version = tagged_version(&[(Some("darwin-aarch64"), "t-1.0.0-darwin-aarch64.mur.zip")]);
        for platform in ["linux-x86_64", "linux-aarch64", "darwin-x86_64"] {
            assert!(select_asset(&version, platform).is_none(), "{platform}");
        }
    }

    #[test]
    fn the_newest_fitting_version_is_chosen() {
        let mut source = MemorySource::new("ana/fits", "ana", 1111, "v1");
        let served = served_tool_run();
        source
            .publish(
                "v1",
                "tool-x",
                "0.1.0",
                None,
                &[&served],
                zip_bytes("tool-x", "0.1.0"),
            )
            .publish(
                "v1",
                "tool-x",
                "0.1.1",
                None,
                &[&served],
                zip_bytes("tool-x", "0.1.1"),
            )
            .publish(
                "v1",
                "tool-x",
                "0.2.0",
                Some(other_platform()),
                &[&served],
                zip_bytes("tool-x", "0.2.0"),
            )
            .publish(
                "v1",
                "tool-x",
                "0.3.0",
                None,
                &["murmur:tool/run@9.9.9"],
                zip_bytes("tool-x", "0.3.0"),
            );
        let artifact = &source.index.artifacts[0];
        assert_eq!(
            newest_fitting(artifact, current_platform())
                .unwrap()
                .version,
            "0.1.1"
        );
        assert_eq!(
            resolve_in(&source, "ana/tool-x", None).unwrap().version,
            "0.1.1"
        );
        // An explicit version that does not fit is refused before anything is fetched, naming
        // the newest that does.
        let fetched_before = source.fetched().len();
        let error = resolve_in(&source, "ana/tool-x", Some("0.3.0")).unwrap_err();
        assert!(
            matches!(
                &*error,
                ResolveError::DoesNotFit { version, fitting: Some(fitting), .. }
                    if version == "0.3.0" && fitting == "0.1.1"
            ),
            "{error:?}"
        );
        assert_eq!(source.fetched().len(), fetched_before);
        let cli = CliError::from(error);
        assert_eq!(cli.code, crate::error::E_WIT_LEVEL_001);
        assert!(
            cli.message.starts_with(
                "artifact 'ana/tool-x@0.3.0' does not fit WIT level 2, the level this mur serves: \
                 it names murmur:tool/run@9.9.9, and level 2 has murmur:tool@0.1.0"
            ),
            "{}",
            cli.message
        );
        assert_eq!(
            cli.hint.as_deref(),
            Some(
                "ana/tool-x@0.1.1 is the newest version in github:ana/fits's index that fits \
                 level 2; pin it in murmur.yaml and run `mur install`, or run `mur install \
                 ana/tool-x@0.1.1`"
            )
        );
        // A deploy fetches it as asked.
        #[cfg(feature = "beta-mur-deploy")]
        {
            let resolved = resolve(
                &source,
                &name("ana/tool-x"),
                Some("0.3.0"),
                ExplicitVersion::AsAsked,
                current_platform(),
                None,
            )
            .unwrap();
            assert_eq!(resolved.version, "0.3.0");
        }
    }

    #[test]
    fn no_fitting_version_names_the_newest_and_why() {
        let mut source = MemorySource::new("ana/fits", "ana", 1111, "v1");
        source.publish(
            "v1",
            "tool-none",
            "1.0.0",
            None,
            &["murmur:tool/run@9.9.9"],
            zip_bytes("tool-none", "1.0.0"),
        );
        let cli = CliError::from(resolve_in(&source, "ana/tool-none", None).unwrap_err());
        assert_eq!(cli.code, E_REG_001);
        assert!(
            cli.message.contains(
                "fits WIT level 2, the level this mur serves; the newest is ana/tool-none: 1.0.0 \
                 needs murmur:tool/run@9.9.9, this mur serves murmur:tool@"
            ),
            "{}",
            cli.message
        );
        let hint = cli.hint.unwrap();
        assert!(
            hint.contains("https://docs.murmur.nexus/reference/wit-levels/")
                && !hint.contains("mur install ana/tool-none@1.0.0"),
            "the hint points at the WIT levels page, not at an explicit version: {hint}"
        );
        assert!(source.fetched().is_empty());
    }

    #[test]
    fn an_index_of_another_scope_is_refused_before_any_fetch() {
        let mut source = ana();
        source.index.scope = "bob".to_string();
        let cli = CliError::from(resolve_in(&source, "ana/tool-git", Some("1.0.0")).unwrap_err());
        assert_eq!(cli.code, E_REG_009);
        assert_eq!(
            cli.message,
            "could not resolve ana/tool-git@1.0.0: github:ana/artifacts publishes an index for \
             scope 'bob', and it serves scope 'ana'"
        );
        assert!(source.fetched().is_empty());
    }

    #[test]
    fn a_digest_or_packed_version_other_than_the_index_lists_is_refused() {
        let mut source = ana();
        source.index.artifacts[0].versions[0].assets[0].sha256 = "f".repeat(64);
        let cli = CliError::from(resolve_in(&source, "ana/tool-git", Some("1.0.0")).unwrap_err());
        assert_eq!(cli.code, E_REG_002);
        assert!(
            cli.message.starts_with(
                "could not resolve ana/tool-git@1.0.0: tool-git-1.0.0.mur.zip from \
                     github:ana/artifacts has sha256"
            ),
            "{}",
            cli.message
        );

        let mut source = MemorySource::new("ana/artifacts", "ana", 1111, "v1");
        source.publish(
            "v1",
            "tool-git",
            "1.0.0",
            None,
            &[],
            zip_bytes("tool-git", "1.0.1"),
        );
        let cli = CliError::from(resolve_in(&source, "ana/tool-git", Some("1.0.0")).unwrap_err());
        assert_eq!(cli.code, E_REG_002);
        assert!(
            cli.message.contains("packs version '1.0.1'"),
            "{}",
            cli.message
        );
    }

    fn lock_with(entries: &[(&str, Option<u64>)]) -> MurmurLock {
        let mut lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: Vec::new(),
        };
        for (name, owner_id) in entries {
            lock.upsert(
                name,
                "1.0.0",
                LockedSha256::any("abc"),
                LockOrigin::Operator,
            );
            if let Some(owner_id) = owner_id {
                lock.set_owner_id(name, *owner_id);
            }
        }
        lock
    }

    #[test]
    fn an_owner_other_than_the_lock_records_is_refused_before_the_index_is_read() {
        let mut source = ana();
        source.index_error = Some(SourceError::Other("the index must not be read".into()));
        source.owner_id = 2222;
        let lock = lock_with(&[("bob/x", Some(9)), ("ana/tool-git", Some(1111))]);
        let error = resolve(
            &source,
            &name("ana/tool-git-validate"),
            Some("1.0.0"),
            ExplicitVersion::FitChecked,
            current_platform(),
            Some(&lock),
        )
        .unwrap_err();
        let cli = CliError::from(error);
        assert_eq!(cli.code, E_REG_010);
        assert_eq!(
            cli.message,
            "could not resolve ana/tool-git-validate@1.0.0: the owner of scope 'ana' has changed: \
             github:ana/artifacts is now owned by GitHub account id 2222, and murmur.lock records \
             owner id 1111 (entry ana/tool-git)"
        );
        assert!(source.fetched().is_empty());
    }

    #[test]
    fn the_murmur_scope_is_held_to_its_compiled_in_owner() {
        let mut source = MemorySource::new(DEFAULT_ARTIFACTS_REPO, MURMUR_SCOPE, 1, "v1");
        source.publish(
            "v1",
            "tool-x",
            "0.1.0",
            None,
            &[],
            zip_bytes("tool-x", "0.1.0"),
        );
        let cli = CliError::from(resolve_in(&source, "murmur/tool-x", Some("0.1.0")).unwrap_err());
        assert_eq!(cli.code, E_REG_010);
        assert!(
            cli.message.contains("190196424") && cli.message.contains("owner id 1"),
            "{}",
            cli.message
        );

        source.owner_id = MURMUR_OWNER_ID;
        assert!(resolve_in(&source, "murmur/tool-x", Some("0.1.0")).is_ok());
    }

    #[test]
    fn a_stored_payload_of_another_owner_is_refused() {
        let lock = lock_with(&[("ana/tool-git", Some(1111))]);
        let tool = name("ana/tool-git");
        assert!(check_stored_owner(&tool, "1.0.0", "the global store", None, Some(&lock)).is_ok());
        assert!(
            check_stored_owner(&tool, "1.0.0", "the global store", Some(1111), Some(&lock)).is_ok()
        );
        let error = check_stored_owner(&tool, "1.0.0", "the global store", Some(2222), Some(&lock))
            .unwrap_err();
        assert_eq!(error.code, E_REG_010);
        assert!(check_stored_owner(
            &name("murmur/x"),
            "1.0.0",
            "the global store",
            Some(5),
            None
        )
        .is_err());
    }

    fn config_error(yaml: &str) -> CliError {
        let registry: crate::config::MurConfig = serde_norway::from_str(yaml).unwrap();
        configured_sources(Path::new("/home/x/.murmur/config.yaml"), &registry.registry)
            .unwrap_err()
    }

    #[test]
    fn every_removed_or_unusable_source_form_is_refused_naming_the_file() {
        for (yaml, needle) in [
            ("registry:\n  default: official\n", "no longer reads: each scope"),
            ("registry:\n  sources:\n    - name: x\n      repo: ana/a\n", "sets name:"),
            ("registry:\n  sources:\n    - type: github\n      repo: ana/a\n", "sets type:"),
            ("registry:\n  sources:\n    - url: https://x\n      repo: ana/a\n", "sets url:"),
            ("registry:\n  sources:\n    - token: abc\n", "names no repo"),
            ("registry:\n  sources:\n    - repo: ana\n", "repo 'ana' is not"),
            ("registry:\n  sources:\n    - repo: ana/x/y\n", "repo 'ana/x/y' is not"),
            ("registry:\n  sources:\n    - repo: murmur/x\n", "reserved scope 'murmur'"),
            ("registry:\n  sources:\n    - repo: local/x\n", "reserved scope 'local'"),
            (
                "registry:\n  sources:\n    - repo: ana/a\n    - repo: Ana/b\n",
                "registry.sources[0] (ana/a) and registry.sources[1] (Ana/b) both serve scope 'ana'",
            ),
        ] {
            let error = config_error(yaml);
            assert_eq!(error.code, E_CFG_003, "{yaml}");
            assert!(
                error.message.starts_with("/home/x/.murmur/config.yaml"),
                "{}",
                error.message
            );
            assert!(error.message.contains(needle), "{needle}: {}", error.message);
            assert!(error.hint.is_some(), "{yaml}");
        }
    }

    #[test]
    fn the_default_artifacts_repository_only_lends_the_built_in_source_its_token() {
        let registry: crate::config::MurConfig = serde_norway::from_str(
            "registry:\n  sources:\n    - repo: murmur-nexus/default-artifacts\n      token: abc\n    - repo: ana/a\n",
        )
        .unwrap();
        let sources = configured_sources(Path::new("c.yaml"), &registry.registry).unwrap();
        assert_eq!(sources[0].scope, MURMUR_SCOPE);
        assert_eq!(sources[0].token.as_deref(), Some("abc"));
        assert_eq!(sources[1].scope, "ana");
    }

    /// `mur deploy` builds its sources from the global file alone, and refuses the same forms
    /// with the same code.
    #[test]
    fn the_global_only_sources_refuse_a_removed_key() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _guard = crate::config::test_env::EnvGuard::set_up(home.path(), cwd.path());
        std::fs::create_dir_all(home.path().join(".murmur")).unwrap();
        let config = home.path().join(".murmur").join("config.yaml");
        std::fs::write(&config, "registry:\n  default: official\n").unwrap();

        let error = match Sources::load_global() {
            Ok(_) => panic!("a removed key must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.code, E_CFG_003);
        assert!(
            error.message.starts_with(&config.display().to_string()),
            "{}",
            error.message
        );

        std::fs::write(&config, "registry:\n  sources: []\n").unwrap();
        let sources = Sources::load_global().unwrap();
        assert!(sources.for_scope(MURMUR_SCOPE).is_some());
        assert!(sources.for_scope("ana").is_none());
        let error = match sources.source_for(&name("ana/tool-x"), Some("1.0.0")) {
            Ok(_) => panic!("no source serves ana"),
            Err(error) => error,
        };
        assert_eq!(error.code, E_REG_008);
    }

    #[test]
    fn each_scope_is_served_by_its_own_source_alone() {
        let sources = Sources::from_sources(vec![std::sync::Arc::new(ana())]);
        let source = sources
            .source_for(&name("ana/tool-git"), Some("1.0.0"))
            .unwrap();
        assert_eq!(source.label(), "github:ana/artifacts");
        let refused = |full: &str| match sources.source_for(&name(full), None) {
            Ok(_) => panic!("{full} has no source"),
            Err(error) => error,
        };
        assert_eq!(refused("anabel/tool-git").code, E_REG_008);
        assert_eq!(refused("local/tool-git").code, E_REG_001);
    }

    #[test]
    fn a_project_source_replaces_the_global_one_for_its_scope() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _guard = crate::config::test_env::EnvGuard::set_up(home.path(), cwd.path());
        for (dir, repo) in [(home.path(), "ana/a"), (cwd.path(), "ana/b")] {
            std::fs::create_dir_all(dir.join(".murmur")).unwrap();
            std::fs::write(
                dir.join(".murmur").join("config.yaml"),
                format!("registry:\n  sources:\n    - repo: {repo}\n"),
            )
            .unwrap();
        }
        let sources = Sources::load().unwrap();
        assert_eq!(sources.for_scope("ana").unwrap().repo(), "ana/b");
        assert_eq!(
            Sources::load_global()
                .unwrap()
                .for_scope("ana")
                .unwrap()
                .repo(),
            "ana/a"
        );
    }

    #[test]
    fn rate_limit_hints_follow_whether_a_token_was_sent() {
        let limited = |authenticated| SourceError::RateLimited {
            status: 403,
            message: "API rate limit exceeded for 127.0.0.1.".to_string(),
            signal: RateLimitSignal::RemainingHeader,
            resets_in_secs: Some(2460),
            authenticated,
        };
        let cli = CliError::from(ResolveError::Unanswered {
            source: "github:ana/artifacts".to_string(),
            target: "ana/tool-git@1.0.0".to_string(),
            error: limited(false),
        });
        assert_eq!(cli.code, E_REG_006);
        assert_eq!(
            cli.message,
            "could not look up ana/tool-git@1.0.0: github:ana/artifacts did not answer — rate \
             limited by GitHub (HTTP 403, x-ratelimit-remaining: 0): API rate limit exceeded for \
             127.0.0.1. — resets in about 41 minutes"
        );
        let hint = cli.hint.unwrap();
        for needle in ["GITHUB_TOKEN", "gh auth token", "token: ${GH_TOKEN}", "60"] {
            assert!(hint.contains(needle), "{needle} missing from {hint}");
        }
        assert_eq!(unanswered_hint(&limited(true)), TOKEN_RATE_LIMIT_HINT);
        assert_eq!(
            unanswered_hint(&SourceError::Http {
                status: 403,
                message: "SAML".to_string()
            }),
            UNANSWERED_HINT
        );
    }

    /// Every resolution error for `ana/tool-gi@1.0.0`, as the user reads it.
    fn every_resolve_error() -> Vec<ResolveError> {
        let source = "github:ana/artifacts".to_string();
        let target = "ana/tool-gi@1.0.0".to_string();
        let tool = name("ana/tool-gi");
        let version = "1.0.0".to_string();
        let file = "tool-gi-1.0.0.mur.zip".to_string();
        vec![
            ResolveError::Unanswered {
                source: source.clone(),
                target: target.clone(),
                error: SourceError::Other("timed out".into()),
            },
            ResolveError::RepositoryMissing {
                source: source.clone(),
                target: target.clone(),
            },
            ResolveError::NameNotListed {
                source: source.clone(),
                release: "v2".into(),
                target: target.clone(),
                name: tool.clone(),
            },
            ResolveError::VersionNotListed {
                source: source.clone(),
                name: tool.clone(),
                version: version.clone(),
                listed: vec!["0.9.0".into()],
            },
            ResolveError::NoPayloadFor {
                source: source.clone(),
                name: tool.clone(),
                version: version.clone(),
                platform: "linux-x86_64".into(),
                published: vec!["darwin-aarch64".into()],
            },
            ResolveError::DoesNotFit {
                source: source.clone(),
                name: tool.clone(),
                version: version.clone(),
                mismatched: Vec::new(),
                fitting: Some("0.9.0".into()),
            },
            ResolveError::NoneFits {
                source: source.clone(),
                name: tool.clone(),
                newest: version.clone(),
                fit: VersionFit::NoPayloadFor("linux-x86_64".into()),
            },
            ResolveError::AssetMissing {
                source: source.clone(),
                target: target.clone(),
                release: "v2".into(),
                file: file.clone(),
                reason: "it is gone".into(),
            },
            ResolveError::NoIndex {
                source: source.clone(),
                repo: "ana/artifacts".into(),
                target: target.clone(),
                release: Some("v2".into()),
            },
            ResolveError::BadIndex {
                source: source.clone(),
                repo: "ana/artifacts".into(),
                target: target.clone(),
                release: "v2".into(),
                error: ArtifactIndexError::Schema1,
            },
            ResolveError::ScopeClaimed {
                source: source.clone(),
                target: target.clone(),
                claimed: "bob".into(),
                scope: "ana".into(),
            },
            ResolveError::OwnerChanged {
                source: source.clone(),
                target: target.clone(),
                scope: "ana".into(),
                current: 2222,
                recorded: RecordedOwner::Lock {
                    entry: "ana/tool-git".into(),
                    owner_id: 1111,
                },
            },
            ResolveError::OwnerChanged {
                source: source.clone(),
                target: target.clone(),
                scope: "ana".into(),
                current: 2222,
                recorded: RecordedOwner::BuiltIn,
            },
            ResolveError::Digest {
                source: source.clone(),
                target: target.clone(),
                file: file.clone(),
                expected: "a".repeat(64),
                actual: "b".repeat(64),
            },
            ResolveError::PackedVersion {
                source: source.clone(),
                file: file.clone(),
                name: tool.clone(),
                version: version.clone(),
                packed: "1.0.1".into(),
            },
            ResolveError::PackedName {
                name: tool.clone(),
                version: version.clone(),
                packed: "tool-gx".into(),
            },
            ResolveError::Unreadable {
                source,
                target,
                file,
                error: "not a zip".into(),
            },
        ]
    }

    #[test]
    fn full_name_in_errors_of_every_resolution() {
        for error in every_resolve_error() {
            let cli = CliError::from(error.clone());
            let text = format!("{}\n{}", cli.message, cli.hint.clone().unwrap_or_default());
            assert!(cli.message.contains("ana/tool-gi"), "{error:?}: {text}");
            assert!(!text.contains("'tool-gi'"), "{error:?}: {text}");
            assert!(!text.contains("registry config"), "{error:?}: {text}");
        }
    }

    #[test]
    fn full_name_in_errors_of_the_store() {
        let tool = name("ana/tool-gi");
        let not_found = CliError::from(RegistryError::NotFound {
            name: tool.to_string(),
            version: "1.0.0".into(),
        });
        assert_eq!(
            not_found.message,
            "artifact ana/tool-gi@1.0.0 is not in the store"
        );
        let staging = CliError::from(capsule_runtime::RuntimeError::ArtifactNotFound {
            name: tool.to_string(),
            version: "1.0.0".into(),
        });
        assert_eq!(staging.code, E_REG_001);
        assert_eq!(
            staging.message,
            "artifact ana/tool-gi@1.0.0 is not in the store"
        );
        assert_eq!(
            staging.hint.as_deref(),
            Some("install it: mur install ana/tool-gi@1.0.0")
        );
        let local = local_store_miss(&name("local/tool-gi"), Some("1.0.0"));
        let local_text = format!("{}\n{}", local.message, local.hint.unwrap_or_default());
        assert!(local.message.contains("local/tool-gi"), "{local_text}");
        for text in [&not_found.message, &staging.message, &local_text] {
            assert!(!text.contains("'tool-gi'"), "{text}");
            assert!(!text.contains("registry"), "{text}");
        }
    }
}
