//! The on-disk artifact stores a session resolves against: the project store
//! `<project>/.murmur/artifacts/` and the global store `~/.murmur/artifacts/`.

use std::fs::File;

use murmur_artifact::{
    wit_contracts_from_artifact_reader, ArtifactMeta, ArtifactName, LocalRegistry, PublishResult,
    Registry, RegistryError, ResolvedArtifact, UnservedInterface, WitContracts,
};

/// Resolves against `primary` first and, on `NotFound` only, falls back to `secondary`.
///
/// A session finds its artifacts in the project store first, then the global one. Both `mur run`
/// and `mur eval` stage against this chain, so an artifact the pre-flight check in
/// `commands::run::artifact_presence` reports as present is also one staging can load; staging
/// against a single store would let the two disagree.
///
/// Only `NotFound` falls through. Any other error is a real failure of the primary store and is
/// returned as such rather than being masked by a lookup somewhere else.
pub(crate) struct FallbackRegistry {
    pub(crate) primary: LocalRegistry,
    pub(crate) secondary: LocalRegistry,
}

impl Registry for FallbackRegistry {
    fn resolve(
        &self,
        name: &ArtifactName,
        version: &str,
    ) -> Result<ResolvedArtifact, RegistryError> {
        match self.primary.resolve(name, version) {
            Err(RegistryError::NotFound { .. }) => self.secondary.resolve(name, version),
            other => other,
        }
    }

    fn resolve_with_platform(
        &self,
        name: &ArtifactName,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        match self.primary.resolve_with_platform(name, version, platform) {
            Err(RegistryError::NotFound { .. }) => self
                .secondary
                .resolve_with_platform(name, version, platform),
            other => other,
        }
    }

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError> {
        self.primary.publish(meta, bytes)
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        self.primary.list_index()
    }
}

/// The interfaces one installed artifact speaks: the `wit_contracts` its sidecar records when the
/// key is present, otherwise derived from its payload.
///
/// The store derives the recorded value from the payload on every write, so a recorded value is
/// used as is. The key is absent for a sidecar written before the field existed, which is the
/// case derivation covers, and for a payload with no component, where derivation finds nothing
/// after one zip central-directory read. Each payload is opened as a file and only its root wasm
/// entry is read. A platform-tagged version directory holds one payload per platform, and their
/// names are unioned.
pub(crate) fn installed_contracts(
    registry: &LocalRegistry,
    meta: &ArtifactMeta,
) -> Option<WitContracts> {
    if let Some(contracts) = &meta.wit_contracts {
        return Some(contracts.clone());
    }

    let generic = registry.artifact_path_for(&meta.name, &meta.version);
    let payloads = if generic.is_file() {
        vec![generic]
    } else {
        let version_dir = generic.parent()?;
        let prefix = format!("{}-{}-", meta.name.name(), meta.version);
        let mut tagged: Vec<_> = std::fs::read_dir(version_dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".mur.zip"))
            })
            .collect();
        tagged.sort();
        tagged
    };

    let mut union: Option<WitContracts> = None;
    for payload in payloads {
        let Some(contracts) = File::open(&payload)
            .ok()
            .and_then(wit_contracts_from_artifact_reader)
        else {
            continue;
        };
        let merged = union.get_or_insert_with(WitContracts::default);
        merged.exports.extend(contracts.exports);
        merged.imports.extend(contracts.imports);
    }
    if let Some(merged) = &mut union {
        for names in [&mut merged.exports, &mut merged.imports] {
            names.sort();
            names.dedup();
        }
    }
    union
}

/// An interface name as a doctor line or a search note shows it: as recorded, or marked when it
/// carries no version.
pub(crate) fn interface_label(interface: &UnservedInterface) -> String {
    match interface.artifact_version {
        Some(_) => interface.interface.clone(),
        None => format!("{} (unversioned)", interface.interface),
    }
}

/// What this `mur` serves of the interface's package, completing a doctor line or a search note.
pub(crate) fn served_phrase(interface: &UnservedInterface) -> String {
    match &interface.served_version {
        Some(served) => format!("this mur serves {}@{served}", interface.package),
        None => format!("this mur serves no version of {}", interface.package),
    }
}
