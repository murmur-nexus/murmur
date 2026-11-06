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

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError> {
        self.primary.publish(meta, bytes)
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        self.primary.list_index()
    }

    /// Both stores' rows, the project store's winning on the same `name@version`, as a resolve
    /// would.
    fn list_stored(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        let mut stored = self.primary.list_index()?;
        for meta in self.secondary.list_index()? {
            if !stored
                .iter()
                .any(|held| held.name == meta.name && held.version == meta.version)
            {
                stored.push(meta);
            }
        }
        Ok(stored)
    }

    /// `project store` when the project store holds `name@version`, else `global store` when the
    /// global one does: the store a resolve takes it from.
    fn held_in(&self, name: &ArtifactName, version: &str) -> Option<String> {
        let holds = |store: &LocalRegistry| {
            store
                .artifact_path_for(name, version)
                .parent()
                .is_some_and(std::path::Path::is_dir)
        };
        if holds(&self.primary) {
            Some(PROJECT_STORE.to_string())
        } else if holds(&self.secondary) {
            Some(GLOBAL_STORE.to_string())
        } else {
            None
        }
    }
}

/// How a pull's `fetched_from` and its messages name the project store.
pub(crate) const PROJECT_STORE: &str = "project store";
/// How a pull's `fetched_from` and its messages name the global store.
pub(crate) const GLOBAL_STORE: &str = "global store";

/// The interfaces one installed artifact speaks: the `wit_contracts` its sidecar records when the
/// key is present, otherwise derived from its payload.
///
/// The store derives the recorded value from the payload on every write, so a recorded value is
/// used as is. The key is absent for a sidecar written before the field existed, which is the
/// case derivation covers, and for a payload with no component, where derivation finds nothing
/// after one zip central-directory read. The payload is opened as a file and only its root wasm
/// entry is read.
pub(crate) fn installed_contracts(
    registry: &LocalRegistry,
    meta: &ArtifactMeta,
) -> Option<WitContracts> {
    if let Some(contracts) = &meta.wit_contracts {
        return Some(contracts.clone());
    }
    let payload = File::open(registry.artifact_path_for(&meta.name, &meta.version)).ok()?;
    let mut contracts = wit_contracts_from_artifact_reader(payload)?;
    for names in [&mut contracts.exports, &mut contracts.imports] {
        names.sort();
        names.dedup();
    }
    Some(contracts)
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
