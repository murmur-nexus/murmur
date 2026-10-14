//! The on-disk artifact stores a session resolves against: the project store
//! `<project>/.murmur/artifacts/` and the global store `~/.murmur/artifacts/`.

use murmur_artifact::{
    ArtifactMeta, ArtifactName, LocalRegistry, PublishResult, Registry, RegistryError,
    ResolvedArtifact,
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
