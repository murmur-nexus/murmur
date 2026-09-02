use std::{
    fs,
    path::{Path, PathBuf},
};

use murmur_artifact::{
    current_platform, is_reserved_version, load_manifest, load_manifest_from_artifact,
    load_manifest_yaml_from_artifact, parse_tool_implementation_from_yaml, resolve_manifest_path,
    ArtifactImplementation, ArtifactMeta, RuntimeType,
};

use crate::{
    config::resolve_registry,
    error::{CliError, E_IO_001, E_IO_003, E_REG_004},
};

pub(crate) fn run_publish(
    artifact_path_arg: Option<&Path>,
    registry_override: Option<&str>,
    platform: Option<&str>,
) -> Result<(), CliError> {
    let artifact_path = resolve_publish_artifact_path(artifact_path_arg)?;
    let bytes = fs::read(&artifact_path).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!(
                "failed to read artifact at {}: {source}",
                artifact_path.display()
            ),
        )
    })?;

    let artifact_manifest = load_manifest_from_artifact(&artifact_path).map_err(|error| {
        CliError::new(
            E_IO_003,
            format!(
                "failed to parse murmur.yaml in {}: {error}",
                artifact_path.display()
            ),
        )
    })?;

    if is_reserved_version(&artifact_manifest.version) {
        return Err(CliError::with_hint(
            E_REG_004,
            format!(
                "version '{}' is reserved and cannot be published",
                artifact_manifest.version
            ),
            "reserved strings: latest, stable, edge — use an explicit semver version",
        ));
    }

    let registry_runtime = artifact_manifest.registry_runtime();

    let (runtime, platforms) = match (platform, registry_runtime) {
        // Explicit platform flag always takes precedence.
        (Some(raw), _) => (RuntimeType::Native, vec![parse_platform(raw)?]),

        // Static artifact — doc-only guidance file, no platform tag.
        (None, RuntimeType::Static) => (RuntimeType::Static, Vec::new()),

        // WASM or native — inspect implementation field to determine platform handling.
        (None, _) => {
            let manifest_yaml =
                load_manifest_yaml_from_artifact(&artifact_path).map_err(|error| {
                    CliError::new(
                        E_IO_003,
                        format!(
                            "failed to read murmur.yaml from {}: {error}",
                            artifact_path.display()
                        ),
                    )
                })?;
            match parse_tool_implementation_from_yaml(&manifest_yaml) {
                // Native artifact with no explicit platform → auto-detect current build host.
                ArtifactImplementation::Native => {
                    if current_platform() == "unknown" {
                        return Err(CliError::with_hint(
                            E_IO_003,
                            "cannot auto-detect platform for native artifact on this host",
                            "pass an explicit platform with --platform <PLATFORM> (e.g. darwin-aarch64)",
                        ));
                    }
                    println!("Platform: {} (auto-detected)", current_platform());
                    let platform_parts = parse_platform(current_platform())?;
                    (RuntimeType::Native, vec![platform_parts])
                }
                // WASM or unspecified implementation → no platform tag.
                ArtifactImplementation::Wasm => (RuntimeType::Wasm, Vec::new()),
            }
        }
    };

    let artifact_id = format!("{}@{}", artifact_manifest.name, artifact_manifest.version);
    let meta = ArtifactMeta {
        name: artifact_manifest.name,
        version: artifact_manifest.version,
        runtime,
        artifact_runtime: artifact_manifest.runtime,
        platforms,
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
    };

    let registry = resolve_registry(registry_override)?;
    registry.publish(meta, &bytes).map_err(CliError::from)?;
    println!("Published {artifact_id}");
    Ok(())
}

fn resolve_publish_artifact_path(artifact_path_arg: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = artifact_path_arg {
        return Ok(path.to_path_buf());
    }

    let cwd = std::env::current_dir().map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current working directory: {source}"),
        )
    })?;
    let manifest = load_manifest(&resolve_manifest_path(&cwd)).map_err(CliError::from)?;
    let inferred = cwd.join(format!("{}-{}.mur.zip", manifest.name, manifest.version));

    if !inferred.exists() {
        return Err(CliError::new(
            E_IO_001,
            format!(
                "default artifact path {} not found. Run 'mur build' first or pass an explicit artifact path.",
                inferred.display()
            ),
        ));
    }

    Ok(inferred)
}

/// Parse a platform string in `os-arch` format (e.g. `darwin-aarch64`) into `(os, arch)`.
pub(crate) fn parse_platform(input: &str) -> Result<(String, String), CliError> {
    let Some((os, arch)) = input.split_once('-') else {
        return Err(CliError::new(
            E_IO_003,
            format!("invalid platform '{input}' (expected os-arch, e.g. darwin-aarch64)"),
        ));
    };

    if os.trim().is_empty() || arch.trim().is_empty() {
        return Err(CliError::new(
            E_IO_003,
            format!("invalid platform '{input}' (expected os-arch, e.g. darwin-aarch64)"),
        ));
    }

    Ok((os.to_string(), arch.to_string()))
}
