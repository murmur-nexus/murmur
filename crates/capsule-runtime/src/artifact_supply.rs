//! Where a runtime pull finds an artifact the session's stores do not hold.
//!
//! A capsule granted `capabilities.install.from: [stores, sources]` may have a pull answered by
//! the GitHub source of the artifact's scope. The runtime has no source client of its own: the
//! launcher hands staging a [`SourceSupply`] (`StageRequest::source_supply`), and the pull path
//! asks it for an index and then for one exact payload. Every refusal the supply returns is a
//! sentence the pull passes on after its own leading token.
//!
//! A supply writes nothing. What a source pull installs reaches `tools/<tool>/`, `murmur.lock`
//! and the compiled-form cache through the same writes a store pull makes, and no store.

use std::fmt;

use murmur_artifact::{ArtifactIndex, ArtifactName, MurmurLock, ResolvedArtifact};

/// The scope sources a session may fetch from.
pub trait SourceSupply: Send + Sync {
    /// `github:<owner>/<repo>` of the source serving `scope`, or `None` when no source serves it.
    fn source_label(&self, scope: &str) -> Option<String>;
    /// Every scope a source serves, for a `*` listing.
    fn scopes(&self) -> Vec<String>;
    /// The index on the latest release of `scope`'s source.
    fn index(&self, scope: &str) -> Result<ArtifactIndex, String>;
    /// `name@version`'s payload for `platform`, fetched from its scope's source and checked
    /// against the index (SHA-256, packed name and version) and the scope's owner id against
    /// `lock`. Writes nothing.
    fn fetch(
        &self,
        name: &ArtifactName,
        version: &str,
        platform: &str,
        lock: &MurmurLock,
    ) -> Result<SourcedArtifact, String>;
}

impl fmt::Debug for dyn SourceSupply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceSupply")
            .field("scopes", &self.scopes())
            .finish()
    }
}

/// A payload a [`SourceSupply`] fetched and checked.
#[derive(Debug, Clone)]
pub struct SourcedArtifact {
    /// The payload as a store resolve returns one: its `meta` is built from the packed manifest
    /// and carries [`Self::owner_id`].
    pub resolved: ResolvedArtifact,
    /// `github:<owner>/<repo>`.
    pub source: String,
    /// GitHub's numeric id of the source repository's owner, already checked.
    pub owner_id: u64,
}
