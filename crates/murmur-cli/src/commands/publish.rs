use std::{
    fs,
    path::{Path, PathBuf},
};

use murmur_artifact::{
    is_reserved_version, load_manifest, load_manifest_from_artifact_bytes, resolve_manifest_path,
    ArtifactMeta, LocalRegistry, Registry, RegistryError, LOCAL_SCOPE,
};

use crate::error::{
    native_payload_refusal, native_tool_install_error, CliError, E_IO_001, E_IO_003, E_REG_004,
};

pub(crate) fn run_publish(artifact_path_arg: Option<&Path>) -> Result<(), CliError> {
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

    let artifact_manifest = load_manifest_from_artifact_bytes(&bytes).map_err(|error| {
        native_payload_refusal(&error, LOCAL_SCOPE, &bytes).unwrap_or_else(|| {
            CliError::new(
                E_IO_003,
                format!(
                    "failed to parse murmur.yaml in {}: {error}",
                    artifact_path.display()
                ),
            )
        })
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

    // A file names no owner it could be checked against, so what it publishes is `local/`.
    let name = murmur_artifact::ArtifactName::from_parts(LOCAL_SCOPE, &artifact_manifest.name)
        .map_err(|error| {
            CliError::new(
                E_IO_003,
                format!("the artifact declares a name that cannot be published: {error}"),
            )
        })?;
    let version = artifact_manifest.version.clone();
    let artifact_id = format!("{name}@{version}");
    let meta = ArtifactMeta {
        name,
        runtime: artifact_manifest.registry_runtime(),
        version: artifact_manifest.version,
        artifact_runtime: artifact_manifest.runtime,
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: None,
        built_at_level: None,
    };

    let global_store = LocalRegistry::from_default_home().map_err(CliError::from)?;
    global_store
        .publish(meta, &bytes)
        .map_err(|error| match error {
            RegistryError::NativeTool { name, field } => {
                native_tool_install_error(&name, &version, &field)
            }
            error => CliError::from(error),
        })?;
    capsule_runtime::report_println!("Published {artifact_id}");
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
