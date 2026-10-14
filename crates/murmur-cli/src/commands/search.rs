use std::collections::BTreeMap;
use std::time::Duration;

use capsule_runtime::SERVED_WIT_PACKAGES;
use murmur_artifact::{
    compare_versions, current_platform, murmur_interfaces, version_fit, ArtifactIndex,
    ArtifactIndexError, ArtifactMeta, LocalRegistry, Registry, VersionFit,
};

use crate::{
    config,
    error::{CliError, E_IDX_002, E_IO_003, SCHEMA_1_INDEX_HINT},
    stores::{installed_contracts, interface_label, served_phrase},
};

// The default public artifact index URL. Overridable via ~/.murmur/config.yaml key
// registry.index_url, or per-command with --index <PATH|URL|local>.
pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/murmur-nexus/default-artifacts/refs/heads/main/artifacts-index.json";

/// One artifact as `mur search` lists it, from an index or from the global store.
struct SearchEntry {
    /// `<scope>/<name>`.
    full_name: String,
    runtime: String,
    description: Option<String>,
    tags: Vec<String>,
    /// Newest first, each with its fit for this `mur` on this host.
    versions: Vec<(String, VersionFit)>,
}

impl SearchEntry {
    fn newest_fitting(&self) -> Option<&str> {
        self.versions
            .iter()
            .find(|(_, fit)| *fit == VersionFit::Fits)
            .map(|(version, _)| version.as_str())
    }

    /// The line under the table saying why the newest version is not the one listed, or `None`
    /// when it is.
    fn note(&self) -> Option<String> {
        let (version, fit) = self.versions.first()?;
        let name = &self.full_name;
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
                    "{name}: {version} needs {}, {}{more}",
                    interface_label(first),
                    served_phrase(first)
                ))
            }
            VersionFit::NoPayloadFor(platform) => {
                Some(format!("{name}: {version} has no payload for {platform}"))
            }
        }
    }

    fn matches(&self, query_lower: &str) -> bool {
        self.full_name.to_lowercase().contains(query_lower)
            || self
                .description
                .as_deref()
                .is_some_and(|d| d.to_lowercase().contains(query_lower))
            || self
                .tags
                .iter()
                .any(|t| t.to_lowercase().contains(query_lower))
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Where `--index` reads the artifact index from.
#[derive(Debug, PartialEq, Eq)]
enum IndexSource<'a> {
    /// `local`: the global store's own listing.
    GlobalStore,
    /// A value starting `http://` or `https://`.
    Url(&'a str),
    /// Any other value: a file path, relative to the working directory or absolute.
    File(&'a str),
}

impl<'a> IndexSource<'a> {
    fn parse(value: &'a str) -> Self {
        if value == "local" {
            Self::GlobalStore
        } else if value.starts_with("http://") || value.starts_with("https://") {
            Self::Url(value)
        } else {
            Self::File(value)
        }
    }
}

pub(crate) fn run_search(query: &str, index: Option<&str>, limit: usize) -> Result<(), CliError> {
    let platform = current_platform();
    let entries = match index.map(IndexSource::parse) {
        Some(IndexSource::GlobalStore) => scan_local(platform)?,
        Some(IndexSource::File(path)) => index_entries(read_index_file(path)?, platform),
        Some(IndexSource::Url(url)) => index_entries(fetch_remote(url)?, platform),
        None => {
            let url = resolve_index_url()?;
            index_entries(fetch_remote(&url)?, platform)
        }
    };

    let query_lower = query.to_lowercase();
    let matches: Vec<&SearchEntry> = entries
        .iter()
        .filter(|entry| entry.matches(&query_lower))
        .take(limit)
        .collect();

    if matches.is_empty() {
        capsule_runtime::report_println!("No results found.");
        return Ok(());
    }

    print_results(&matches);
    Ok(())
}

// ── Index sources ─────────────────────────────────────────────────────────────

fn resolve_index_url() -> Result<String, CliError> {
    let config = config::load_effective_mur_config()?;
    Ok(config
        .registry
        .index_url
        .unwrap_or_else(|| DEFAULT_INDEX_URL.to_string()))
}

/// `E-IDX-002` for an index read from `source` (a path or URL) that this `mur` cannot use.
pub(crate) fn index_error(source: &str, error: ArtifactIndexError) -> CliError {
    let message = format!("artifact index {source} {error}");
    match error {
        ArtifactIndexError::Schema1 => CliError::with_hint(E_IDX_002, message, SCHEMA_1_INDEX_HINT),
        _ => CliError::new(E_IDX_002, message),
    }
}

/// Read a schema 2 index from a file: `E-IO-003` when it cannot be read, `E-IDX-002` when it is
/// not an index this `mur` reads.
pub(crate) fn read_index_file(path: &str) -> Result<ArtifactIndex, CliError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read artifact index from {path}: {e}"),
        )
    })?;
    ArtifactIndex::from_json(&raw).map_err(|error| index_error(path, error))
}

fn fetch_remote(url: &str) -> Result<ArtifactIndex, CliError> {
    let client = crate::http::blocking_agent(Duration::from_secs(30));
    let mut response = client.get(url).call().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to fetch artifact index from {url}: {e}"),
        )
    })?;

    if !response.status().is_success() {
        return Err(CliError::new(
            E_IO_003,
            format!(
                "failed to fetch artifact index from {url}: HTTP {}",
                response.status()
            ),
        ));
    }

    let raw = response.body_mut().read_to_string().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to fetch artifact index from {url}: {e}"),
        )
    })?;
    ArtifactIndex::from_json(&raw).map_err(|error| index_error(url, error))
}

fn index_entries(index: ArtifactIndex, platform: &str) -> Vec<SearchEntry> {
    let scope = index.scope;
    index
        .artifacts
        .into_iter()
        .map(|artifact| SearchEntry {
            full_name: format!("{scope}/{}", artifact.name),
            versions: artifact
                .versions
                .iter()
                .map(|v| (v.version.clone(), v.fit(SERVED_WIT_PACKAGES, platform)))
                .collect(),
            runtime: artifact.runtime,
            description: artifact.description,
            tags: artifact.tags,
        })
        .collect()
}

/// The global store as search entries, one per full name. A version's interfaces are the ones
/// `mur doctor` reads for it, and its payloads are the platforms its sidecars record, or one
/// untagged payload when they record none.
fn scan_local(platform: &str) -> Result<Vec<SearchEntry>, CliError> {
    let registry = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let mut by_name: BTreeMap<String, Vec<ArtifactMeta>> = BTreeMap::new();
    for meta in registry.list_index().map_err(CliError::from)? {
        by_name.entry(meta.name.to_string()).or_default().push(meta);
    }

    Ok(by_name
        .into_iter()
        .map(|(full_name, mut metas)| {
            metas.sort_by(|a, b| compare_versions(&b.version, &a.version));
            let versions = metas
                .iter()
                .map(|meta| {
                    let interfaces = installed_contracts(&registry, meta)
                        .map(|contracts| murmur_interfaces(&contracts))
                        .unwrap_or_default();
                    let tags: Vec<String> = meta
                        .platforms
                        .iter()
                        .map(|(os, arch)| format!("{os}-{arch}"))
                        .collect();
                    let fit = if tags.is_empty() {
                        version_fit(&interfaces, [None], SERVED_WIT_PACKAGES, platform)
                    } else {
                        version_fit(
                            &interfaces,
                            tags.iter().map(|tag| Some(tag.as_str())),
                            SERVED_WIT_PACKAGES,
                            platform,
                        )
                    };
                    (meta.version.clone(), fit)
                })
                .collect();
            let newest = metas.swap_remove(0);
            SearchEntry {
                full_name,
                runtime: newest.artifact_runtime,
                description: newest.description,
                tags: newest.tags,
                versions,
            }
        })
        .collect())
}

// ── Output ────────────────────────────────────────────────────────────────────

fn print_results(entries: &[&SearchEntry]) {
    const H_NAME: &str = "NAME";
    const H_VERSION: &str = "VERSION";
    const H_RUNTIME: &str = "RUNTIME";
    const H_DESC: &str = "DESCRIPTION";
    const NONE: &str = "\u{2014}";

    let rows: Vec<[&str; 4]> = entries
        .iter()
        .map(|e| {
            [
                e.full_name.as_str(),
                e.newest_fitting().unwrap_or(NONE),
                e.runtime.as_str(),
                e.description.as_deref().unwrap_or(NONE),
            ]
        })
        .collect();
    let width = |column: usize, header: &str| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
            .max(header.len())
    };
    let nw = width(0, H_NAME);
    let vw = width(1, H_VERSION);
    let rw = width(2, H_RUNTIME);

    capsule_runtime::report_println!("{H_NAME:<nw$}  {H_VERSION:<vw$}  {H_RUNTIME:<rw$}  {H_DESC}");
    for [name, version, runtime, desc] in &rows {
        capsule_runtime::report_println!("{name:<nw$}  {version:<vw$}  {runtime:<rw$}  {desc}");
    }

    let notes: Vec<String> = entries.iter().filter_map(|e| e.note()).collect();
    if !notes.is_empty() {
        capsule_runtime::report_println!();
        for note in notes {
            capsule_runtime::report_println!("{note}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IndexSource;

    #[test]
    fn local_names_the_global_store() {
        assert_eq!(IndexSource::parse("local"), IndexSource::GlobalStore);
    }

    #[test]
    fn an_http_or_https_value_is_a_url() {
        for url in [
            "http://127.0.0.1:8080/artifacts-index.json",
            "https://example.com/artifacts-index.json",
        ] {
            assert_eq!(IndexSource::parse(url), IndexSource::Url(url));
        }
    }

    #[test]
    fn any_other_value_is_a_file_path() {
        for path in [
            "./artifacts-index.json",
            "artifacts-index.json",
            "../index/artifacts-index.json",
            "/abs/artifacts-index.json",
            "localhost/artifacts-index.json",
        ] {
            assert_eq!(IndexSource::parse(path), IndexSource::File(path));
        }
    }
}
