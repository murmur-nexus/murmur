//! The declared program needs a launch of this capsule would grant, read from the stores without
//! staging: for `mur run --explain-scope` and `mur doctor`.

use std::path::{Path, PathBuf};

use capsule_runtime::declared_needs::{DeclaredToolNeeds, ResolvedProgramNeeds};
use capsule_runtime::RuntimeError;
use murmur_artifact::{
    ArtifactName, ArtifactRuntime, LocalRegistry, LockOrigin, MurmurLock, Registry, RuntimeArtifact,
};

use crate::error::CliError;
use crate::stores::FallbackRegistry;

/// The needs [`capsule_runtime::declared_needs::derive_declared_needs`] derives for `artifacts`,
/// each `runtime: tool` entry read from `<project_dir>/.murmur/artifacts`, then the global store,
/// at the version and with the lock origin `lock` pins (the manifest's version and `operator`
/// with no lock, as staging reads them). `pinned` is the source choice for pinned program
/// downloads ([`super::pinned_programs::installed_pinned_programs`]), which the needs resolve
/// against.
///
/// Read-only: nothing is pulled, compiled or written. A tool neither store holds, or one `lock`
/// does not pin, contributes nothing: the launch would refuse it before staging. A malformed
/// `programs:` block refuses with `E-RUN-062`, as the launch would.
pub(crate) fn installed_declared_needs(
    artifacts: &[RuntimeArtifact],
    lock: Option<&MurmurLock>,
    project_dir: &Path,
    shell_allow: &[String],
    capsule_writable: &[&Path],
    pinned: &[capsule_runtime::pinned_programs::ResolvedPinnedProgram],
) -> Result<Vec<ResolvedProgramNeeds>, CliError> {
    let registry = FallbackRegistry {
        primary: LocalRegistry::new(project_dir.join(".murmur").join("artifacts")),
        secondary: LocalRegistry::from_default_home().map_err(CliError::from)?,
    };
    let mut tools = Vec::new();
    for artifact in artifacts {
        if artifact.runtime != ArtifactRuntime::Tool || artifact.source.is_some() {
            continue;
        }
        let (version, origin) = match lock {
            Some(lock) => match lock.artifact_for(&artifact.name) {
                Some(entry) => (entry.resolved_version.clone(), entry.origin.clone()),
                None => continue,
            },
            None => (artifact.version.clone(), LockOrigin::Operator),
        };
        let Ok(name) = ArtifactName::parse(&artifact.name) else {
            continue;
        };
        let Ok(resolved) = registry.resolve(&name, &version) else {
            continue;
        };
        let Ok(manifest_yaml) =
            murmur_artifact::load_manifest_yaml_from_artifact_bytes(&resolved.bytes)
        else {
            continue;
        };
        if !manifest_yaml.contains(murmur_artifact::PROGRAMS_KEY) {
            continue;
        }
        let programs = murmur_artifact::parse_program_needs(&manifest_yaml).map_err(|error| {
            CliError::from(RuntimeError::ProgramNeedsMalformed {
                artifact: format!("{}@{version}", artifact.name),
                reason: error.to_string(),
            })
        })?;
        tools.push(DeclaredToolNeeds {
            artifact: artifact.name.clone(),
            origin,
            programs,
        });
    }
    Ok(capsule_runtime::declared_needs::derive_declared_needs(
        &tools,
        shell_allow,
        capsule_writable,
        pinned,
    ))
}

/// The trees a launch with `workdir_root` lets the capsule's commands write, owned, for
/// [`installed_declared_needs`]. `None` when `HOME` cannot hold the records tree, which the launch
/// would refuse.
pub(crate) fn capsule_writable_trees(workdir_root: &Path) -> Option<Vec<PathBuf>> {
    let placement = capsule_runtime::runtime_dir_placement(workdir_root).ok()?;
    Some(
        capsule_runtime::capsule_writable_roots(&placement)
            .iter()
            .map(|root| root.to_path_buf())
            .collect(),
    )
}
