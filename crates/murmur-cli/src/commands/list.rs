use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry};

use crate::{commands::install::find_project_root, error::CliError};

// ── Entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run_list(global: bool, all: bool) -> Result<(), CliError> {
    if all {
        return run_list_all();
    }

    let store = resolve_store(global)?;
    let index = store.list_index().map_err(CliError::from)?;

    if index.is_empty() {
        println!("No artifacts found.");
        return Ok(());
    }

    print_artifact_table(&index);
    Ok(())
}

// ── Store resolution ──────────────────────────────────────────────────────────

/// Returns the project store when inside a project dir, global store otherwise.
/// With `global = true`, always returns the global store.
fn resolve_store(global: bool) -> Result<LocalRegistry, CliError> {
    if global {
        return LocalRegistry::from_default_home().map_err(CliError::from);
    }
    match find_project_root() {
        Ok(root) => Ok(LocalRegistry::new(root.join(".murmur").join("artifacts"))),
        Err(_) => LocalRegistry::from_default_home().map_err(CliError::from),
    }
}

// ── --all: both stores with SCOPE column ─────────────────────────────────────

fn run_list_all() -> Result<(), CliError> {
    let mut entries: Vec<(String, ArtifactMeta)> = Vec::new();

    // Project store first (if we're in a project).
    if let Ok(root) = find_project_root() {
        let store = LocalRegistry::new(root.join(".murmur").join("artifacts"));
        if let Ok(mut index) = store.list_index() {
            index.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
            for meta in index {
                entries.push(("project".to_string(), meta));
            }
        }
    }

    // Global store second.
    let global_store = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let mut global_index = global_store.list_index().map_err(CliError::from)?;
    global_index.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    for meta in global_index {
        entries.push(("global".to_string(), meta));
    }

    if entries.is_empty() {
        println!("No artifacts found.");
        return Ok(());
    }

    print_artifact_table_scoped(&entries);
    Ok(())
}

// ── Table formatting ──────────────────────────────────────────────────────────

pub(crate) fn print_artifact_table(index: &[ArtifactMeta]) {
    const H_NAME: &str = "NAME";
    const H_VERSION: &str = "VERSION";
    const H_RUNTIME: &str = "RUNTIME";
    const H_PLATFORMS: &str = "PLATFORMS";

    let nw = index
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or(0)
        .max(H_NAME.len());
    let vw = index
        .iter()
        .map(|m| m.version.len())
        .max()
        .unwrap_or(0)
        .max(H_VERSION.len());
    let rw = index
        .iter()
        .map(|m| m.artifact_runtime.len())
        .max()
        .unwrap_or(0)
        .max(H_RUNTIME.len());

    println!("{H_NAME:<nw$}  {H_VERSION:<vw$}  {H_RUNTIME:<rw$}  {H_PLATFORMS}");
    for meta in index {
        let platforms = format_platforms(&meta.platforms);
        println!(
            "{name:<nw$}  {version:<vw$}  {runtime:<rw$}  {platforms}",
            name = meta.name,
            version = meta.version,
            runtime = meta.artifact_runtime,
        );
    }
}

fn print_artifact_table_scoped(entries: &[(String, ArtifactMeta)]) {
    const H_SCOPE: &str = "SCOPE";
    const H_NAME: &str = "NAME";
    const H_VERSION: &str = "VERSION";
    const H_RUNTIME: &str = "RUNTIME";
    const H_PLATFORMS: &str = "PLATFORMS";

    let sw = entries
        .iter()
        .map(|(s, _)| s.len())
        .max()
        .unwrap_or(0)
        .max(H_SCOPE.len());
    let nw = entries
        .iter()
        .map(|(_, m)| m.name.len())
        .max()
        .unwrap_or(0)
        .max(H_NAME.len());
    let vw = entries
        .iter()
        .map(|(_, m)| m.version.len())
        .max()
        .unwrap_or(0)
        .max(H_VERSION.len());
    let rw = entries
        .iter()
        .map(|(_, m)| m.artifact_runtime.len())
        .max()
        .unwrap_or(0)
        .max(H_RUNTIME.len());

    println!("{H_SCOPE:<sw$}  {H_NAME:<nw$}  {H_VERSION:<vw$}  {H_RUNTIME:<rw$}  {H_PLATFORMS}");
    for (scope, meta) in entries {
        let platforms = format_platforms(&meta.platforms);
        println!(
            "{scope:<sw$}  {name:<nw$}  {version:<vw$}  {runtime:<rw$}  {platforms}",
            name = meta.name,
            version = meta.version,
            runtime = meta.artifact_runtime,
        );
    }
}

fn format_platforms(platforms: &[(String, String)]) -> String {
    if platforms.is_empty() {
        return String::from("\u{2014}"); // em dash
    }
    platforms
        .iter()
        .map(|(os, arch)| format!("{os}-{arch}"))
        .collect::<Vec<_>>()
        .join(", ")
}
