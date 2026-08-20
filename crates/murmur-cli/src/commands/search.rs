use std::time::Duration;

use serde::Deserialize;

use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry};

use crate::{
    config,
    error::{CliError, E_IO_003},
};

// The default public artifact index URL. Overridable via ~/.murmur/config.yaml key
// registry.index_url, or per-command with --registry <url>.
pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/murmur-nexus/default-artifacts/refs/heads/main/artifacts-index.json";

// ── Index types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ArtifactIndex {
    pub schema_version: String,
    #[allow(dead_code)]
    pub updated_at: String,
    pub artifacts: Vec<ArtifactIndexEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactIndexEntry {
    pub name: String,
    pub version: String,
    pub runtime: String,
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub platforms: Vec<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run_search(
    query: &str,
    registry: Option<&str>,
    limit: usize,
) -> Result<(), CliError> {
    let entries: Vec<ArtifactIndexEntry> = match registry {
        Some("local") => scan_local()?,
        Some(path) if path.starts_with('/') => read_local_file(path)?,
        Some(url) => fetch_remote(url)?,
        None => {
            let url = resolve_index_url()?;
            fetch_remote(&url)?
        }
    };

    let query_lower = query.to_ascii_lowercase();
    let matches: Vec<&ArtifactIndexEntry> = entries
        .iter()
        .filter(|e| matches_query(e, &query_lower))
        .take(limit)
        .collect();

    if matches.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    print_results(&matches);
    Ok(())
}

// ── Registry modes ────────────────────────────────────────────────────────────

fn resolve_index_url() -> Result<String, CliError> {
    let config = config::load_effective_mur_config()?;
    Ok(config
        .registry
        .index_url
        .unwrap_or_else(|| DEFAULT_INDEX_URL.to_string()))
}

fn read_local_file(path: &str) -> Result<Vec<ArtifactIndexEntry>, CliError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read artifact index from {path}: {e}"),
        )
    })?;
    let index: ArtifactIndex = serde_json::from_str(&raw).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to parse artifact index from {path}: {e}"),
        )
    })?;
    if index.schema_version != "1" {
        return Err(CliError::new(
            E_IO_003,
            format!(
                "unsupported artifact index schema version '{}' from {path} (expected 1)",
                index.schema_version
            ),
        ));
    }
    Ok(index.artifacts)
}

fn fetch_remote(url: &str) -> Result<Vec<ArtifactIndexEntry>, CliError> {
    let client = crate::registry_client::blocking_agent(Duration::from_secs(30));
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

    let index: ArtifactIndex = response.body_mut().read_json().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to parse artifact index from {url}: {e}"),
        )
    })?;

    if index.schema_version != "1" {
        return Err(CliError::new(
            E_IO_003,
            format!(
                "unsupported artifact index schema version '{}' from {url} (expected 1)",
                index.schema_version
            ),
        ));
    }

    Ok(index.artifacts)
}

fn scan_local() -> Result<Vec<ArtifactIndexEntry>, CliError> {
    let reg = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let index: Vec<ArtifactMeta> = reg.list_index().map_err(CliError::from)?;
    Ok(index.into_iter().map(meta_to_entry).collect())
}

fn meta_to_entry(meta: ArtifactMeta) -> ArtifactIndexEntry {
    ArtifactIndexEntry {
        name: meta.name,
        version: meta.version,
        runtime: meta.artifact_runtime,
        description: meta.description,
        tags: meta.tags,
        platforms: meta
            .platforms
            .into_iter()
            .map(|(os, arch)| format!("{os}-{arch}"))
            .collect(),
    }
}

// ── Filtering ─────────────────────────────────────────────────────────────────

fn matches_query(entry: &ArtifactIndexEntry, query_lower: &str) -> bool {
    if entry.name.to_ascii_lowercase().contains(query_lower) {
        return true;
    }
    if let Some(desc) = &entry.description {
        if desc.to_ascii_lowercase().contains(query_lower) {
            return true;
        }
    }
    entry
        .tags
        .iter()
        .any(|t| t.to_ascii_lowercase().contains(query_lower))
}

// ── Output ────────────────────────────────────────────────────────────────────

fn print_results(entries: &[&ArtifactIndexEntry]) {
    const H_NAME: &str = "NAME";
    const H_VERSION: &str = "VERSION";
    const H_RUNTIME: &str = "RUNTIME";
    const H_DESC: &str = "DESCRIPTION";

    let nw = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0)
        .max(H_NAME.len());
    let vw = entries
        .iter()
        .map(|e| e.version.len())
        .max()
        .unwrap_or(0)
        .max(H_VERSION.len());
    let rw = entries
        .iter()
        .map(|e| e.runtime.len())
        .max()
        .unwrap_or(0)
        .max(H_RUNTIME.len());

    println!("{H_NAME:<nw$}  {H_VERSION:<vw$}  {H_RUNTIME:<rw$}  {H_DESC}");

    for entry in entries {
        let desc = entry.description.as_deref().unwrap_or("\u{2014}"); // em dash for missing
        let name = &entry.name;
        let version = &entry.version;
        let runtime = &entry.runtime;
        println!("{name:<nw$}  {version:<vw$}  {runtime:<rw$}  {desc}");
    }
}
