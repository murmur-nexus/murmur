//! The scope sources a running session fetches from, for a capsule granted
//! `capabilities.install.from: [stores, sources]`.
//!
//! [`CliSourceSupply`] answers the runtime's [`SourceSupply`] questions with the same sources
//! and the same checks `mur install` uses: one source per scope ([`Sources::load`]), its latest
//! release's index, and [`resolve`] for one exact version. It writes nothing; the runtime installs
//! what it returns into the session and pins it in `murmur.lock`, and no store gains it.

use std::sync::Arc;

use capsule_runtime::{SourceSupply, SourcedArtifact};
use murmur_artifact::{
    current_platform, ArtifactIndex, ArtifactName, MurmurLock, PlatformMatch, ResolvedArtifact,
};

use super::{resolve, ArtifactSource, ExplicitVersion, ResolveError, SourceError, Sources};
use crate::error::CliError;

/// [`SourceSupply`] over the configured sources.
pub(crate) struct CliSourceSupply {
    sources: Sources,
}

impl CliSourceSupply {
    /// The sources `mur install` reads: the global config file, then the project one. A file that
    /// cannot be read or names a removed form fails the launch.
    pub(crate) fn load() -> Result<Self, CliError> {
        Ok(Self {
            sources: Sources::load()?,
        })
    }

    /// A supply over `sources`, for a launch that already loaded them.
    #[cfg(test)]
    pub(crate) fn new(sources: Sources) -> Self {
        Self { sources }
    }

    /// The supply a launch hands staging: `Some` exactly when the capsule may fetch from a
    /// source.
    pub(crate) fn for_policy(
        policy: &capsule_runtime::CapabilityPolicy,
    ) -> Result<Option<Arc<dyn SourceSupply>>, CliError> {
        if !policy.install_from_sources {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self::load()?)))
    }
}

/// `[<code>] <message>; hint: <hint>`, the one line a runtime refusal carries a CLI error in.
fn one_line(error: CliError) -> String {
    match error.hint {
        Some(hint) => format!("[{}] {}; hint: {hint}", error.code, error.message),
        None => format!("[{}] {}", error.code, error.message),
    }
}

/// The CLI error of a request `source` did not answer, on [`resolve`]'s terms.
fn unanswered(source: &dyn ArtifactSource, target: &str, error: SourceError) -> CliError {
    let label = source.label().to_string();
    let target = target.to_string();
    CliError::from(match error {
        SourceError::NotFound(_) => ResolveError::RepositoryMissing {
            source: label,
            target,
        },
        SourceError::NoIndex { release } => ResolveError::NoIndex {
            source: label,
            repo: source.repo().to_string(),
            target,
            release,
        },
        SourceError::BadIndex { release, error } => ResolveError::BadIndex {
            source: label,
            repo: source.repo().to_string(),
            target,
            release,
            error,
        },
        error => ResolveError::Unanswered {
            source: label,
            target,
            error,
        },
    })
}

impl SourceSupply for CliSourceSupply {
    fn source_label(&self, scope: &str) -> Option<String> {
        self.sources
            .for_scope(scope)
            .map(|source| source.label().to_string())
    }

    fn scopes(&self) -> Vec<String> {
        self.sources.scopes()
    }

    fn index(&self, scope: &str) -> Result<ArtifactIndex, String> {
        let source = self
            .sources
            .for_scope(scope)
            .ok_or_else(|| format!("no source serves the '{scope}' scope"))?;
        let target = format!("{scope}/*");
        let published = source
            .index()
            .map_err(|error| one_line(unanswered(source.as_ref(), &target, error)))?;
        if published.index.scope != source.scope() {
            return Err(one_line(CliError::from(ResolveError::ScopeClaimed {
                source: source.label().to_string(),
                target,
                claimed: published.index.scope.clone(),
                scope: source.scope().to_string(),
            })));
        }
        Ok(published.index.clone())
    }

    fn fetch(
        &self,
        name: &ArtifactName,
        version: &str,
        platform: &str,
        lock: &MurmurLock,
    ) -> Result<SourcedArtifact, String> {
        let source = self
            .sources
            .source_for(name, Some(version))
            .map_err(one_line)?;
        let resolved = resolve(
            source.as_ref(),
            name,
            Some(version),
            ExplicitVersion::FitChecked,
            platform,
            Some(lock),
        )
        .map_err(|error| one_line(CliError::from(error)))?;
        let platform_match = if resolved.platform.as_deref() == Some(current_platform()) {
            PlatformMatch::Tagged
        } else {
            PlatformMatch::NotApplicable
        };
        let meta = crate::commands::install::source_meta(
            name,
            &resolved.version,
            &resolved.manifest,
            resolved.platform.as_deref(),
            resolved.owner_id,
        );
        Ok(SourcedArtifact {
            resolved: ResolvedArtifact {
                meta,
                bytes: resolved.bytes,
                sha256: resolved.sha256,
                platform_match,
            },
            source: resolved.source,
            owner_id: resolved.owner_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use murmur_artifact::{LockOrigin, LockedSha256, RuntimeType, LOCK_VERSION};
    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::super::memory::MemorySource;
    use super::*;

    fn tool_zip(name: &str, version: &str) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            zip.start_file("murmur.yaml", SimpleFileOptions::default())
                .unwrap();
            write!(zip, "name: {name}\nversion: {version}\nruntime: tool\n").unwrap();
            zip.start_file("tool.wasm", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"\0asm\x01\0\0\0").unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn supply(source: MemorySource) -> CliSourceSupply {
        CliSourceSupply::new(Sources::from_sources(vec![Arc::new(source)]))
    }

    fn empty_lock() -> MurmurLock {
        MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn a_fetch_carries_the_owner_and_writes_the_meta_a_store_resolve_would() {
        let mut source = MemorySource::new("ana/artifacts", "ana", 4242, "v1");
        source.publish(
            "v1",
            "tool-editor",
            "0.3.0",
            None,
            &[],
            tool_zip("tool-editor", "0.3.0"),
        );
        let supply = supply(source);
        let name = ArtifactName::parse("ana/tool-editor").unwrap();

        assert_eq!(
            supply.source_label("ana").as_deref(),
            Some("github:ana/artifacts")
        );
        assert_eq!(supply.source_label("bob"), None);
        assert!(supply.scopes().contains(&"ana".to_string()));
        assert_eq!(supply.index("ana").unwrap().artifacts.len(), 1);

        let sourced = supply
            .fetch(&name, "0.3.0", current_platform(), &empty_lock())
            .unwrap();
        assert_eq!(sourced.source, "github:ana/artifacts");
        assert_eq!(sourced.owner_id, 4242);
        assert_eq!(sourced.resolved.meta.owner_id, Some(4242));
        assert_eq!(sourced.resolved.meta.runtime, RuntimeType::Wasm);
        assert_eq!(sourced.resolved.meta.artifact_runtime, "tool");
        assert_eq!(sourced.resolved.meta.version, "0.3.0");
    }

    /// A refusal is one line carrying the CLI error's code, message and hint.
    #[test]
    fn a_refused_fetch_is_one_line_with_its_code_and_hint() {
        let mut source = MemorySource::new("ana/artifacts", "ana", 4242, "v1");
        source.publish(
            "v1",
            "tool-editor",
            "0.3.0",
            None,
            &[],
            tool_zip("tool-editor", "0.3.0"),
        );
        let supply = supply(source);
        let name = ArtifactName::parse("ana/tool-editor").unwrap();
        let mut lock = empty_lock();
        lock.upsert(
            "ana/tool-other",
            "1.0.0",
            LockedSha256::any("0".repeat(64)),
            LockOrigin::Operator,
        );
        lock.set_owner_id("ana/tool-other", 1111);

        let refusal = supply
            .fetch(&name, "0.3.0", current_platform(), &lock)
            .unwrap_err();

        assert!(
            refusal.starts_with(
                "[E-REG-010] could not resolve ana/tool-editor@0.3.0: the owner of scope 'ana' \
                 has changed: "
            ),
            "{refusal}"
        );
        assert!(refusal.contains("; hint: "), "{refusal}");
        assert_eq!(
            supply.index("bob").unwrap_err(),
            "no source serves the 'bob' scope"
        );
    }
}
