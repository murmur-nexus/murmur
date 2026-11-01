//! The default-artifact versions `mur new` writes: for each artifact, the newest version that fits
//! this `mur` in a published artifact index, chosen by [`crate::source::newest_fitting`] as
//! `mur install <scope>/<name>` chooses it.
//!
//! The index is the one on the latest default-artifacts release, read through the built-in
//! `murmur` source `mur install` reads it from, or the file or URL `--index` names. With no index
//! to read, or no fitting version in it, `mur new` refuses: it has no version of its own to fall
//! back on.

use std::time::Duration;

use murmur_artifact::{current_platform, ArtifactIndex, ArtifactName, MURMUR_SCOPE};

use crate::commands::search::{fetch_index_text, index_error, IndexSource};
use crate::error::{CliError, E_NEW_005, E_NEW_006, E_NEW_007};
use crate::source::{
    fit_note, index_fit, newest_fitting, unanswered_hint, ArtifactSource, SourceError, Sources,
};

/// The hint of every `E-NEW-006`.
const INDEX_HINT: &str = "pass --index <PATH|URL> to take versions from an artifact index file or \
     http(s) URL instead";

/// How long reading an `--index` URL may take.
const INDEX_URL_TIMEOUT: Duration = Duration::from_secs(30);

/// One published artifact index, and how messages name it.
#[derive(Debug)]
pub(crate) struct DefaultVersions {
    /// `<source label> <release>` for the default index, else the `--index` value as given.
    label: String,
    index: ArtifactIndex,
}

impl DefaultVersions {
    /// The index `index` names: `None` is the latest default-artifacts release, `local` is refused
    /// `E-NEW-005` before anything is read, an `http(s)` URL is fetched on the host, and any other
    /// value is a file read on the host.
    ///
    /// `E-NEW-006` when the index cannot be read, `E-IDX-002` when it is not a schema 2 index, and
    /// `E-NEW-007` when the default source's index serves another scope.
    pub(crate) fn load(index: Option<&str>) -> Result<Self, CliError> {
        refuse_global_store(index)?;
        let Some(index) = index else {
            return Self::from_default_source();
        };
        match IndexSource::parse(index) {
            IndexSource::GlobalStore => unreachable!("refuse_global_store refuses `local`"),
            IndexSource::Url(url) => {
                let text = fetch_index_text(url, INDEX_URL_TIMEOUT).map_err(|reason| {
                    CliError::with_hint(
                        E_NEW_006,
                        format!("could not read the artifact index at {url}: {reason}"),
                        INDEX_HINT,
                    )
                })?;
                Self::from_text(url, &text)
            }
            IndexSource::File(path) => Self::from_text(path, &read_index_text(path)?),
        }
    }

    /// The index `text` holds, named `label` in messages. `E-IDX-002` when it is not a schema 2
    /// index.
    pub(crate) fn from_text(label: &str, text: &str) -> Result<Self, CliError> {
        let index = ArtifactIndex::from_json(text).map_err(|error| index_error(label, error))?;
        Ok(Self {
            label: label.to_string(),
            index,
        })
    }

    fn from_default_source() -> Result<Self, CliError> {
        let source = Sources::load()?
            .for_scope(MURMUR_SCOPE)
            .expect("Sources always holds the built-in murmur source");
        Self::from_source(source.as_ref())
    }

    /// The index on `source`'s latest release. No owner-id request: `mur install` checks the
    /// owner when the printed install step runs.
    fn from_source(source: &dyn ArtifactSource) -> Result<Self, CliError> {
        let published = source
            .index()
            .map_err(|error| source_unread(source.label(), &error))?;
        let label = format!("{} {}", source.label(), published.release);
        if published.index.scope != MURMUR_SCOPE {
            return Err(CliError::with_hint(
                E_NEW_007,
                format!(
                    "the artifact index of {label} lists the '{}' scope, not '{MURMUR_SCOPE}'",
                    published.index.scope
                ),
                INDEX_HINT,
            ));
        }
        Ok(Self {
            label,
            index: published.index.clone(),
        })
    }

    /// How messages name the index: `github:murmur-nexus/default-artifacts <release tag>`, or the
    /// `--index` value as given.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// The newest version of `full_name` (`<scope>/<name>`) that fits this `mur` on this host.
    /// `E-NEW-007` when the index is of another scope, does not list the artifact, or lists no
    /// version of it that fits.
    pub(crate) fn newest_fitting(&self, full_name: &str) -> Result<String, CliError> {
        let label = &self.label;
        let no_version = |message: String| {
            CliError::with_hint(
                E_NEW_007,
                message,
                format!(
                    "upgrade mur, or pass --index <PATH|URL> naming an artifact index that lists a \
                     version of {full_name} this mur fits"
                ),
            )
        };
        let name = ArtifactName::parse(full_name)
            .map_err(|error| no_version(format!("{full_name} is not an artifact name: {error}")))?;
        if name.scope() != self.index.scope {
            return Err(no_version(format!(
                "the artifact index {label} lists the '{}' scope, so it has no version of \
                 {full_name}",
                self.index.scope
            )));
        }
        let Some(artifact) = self
            .index
            .artifacts
            .iter()
            .find(|artifact| artifact.name == name.name())
        else {
            return Err(no_version(format!(
                "the artifact index {label} lists no {full_name}"
            )));
        };
        let platform = current_platform();
        if let Some(fitting) = newest_fitting(artifact, platform) {
            return Ok(fitting.version.clone());
        }
        let mut message = format!(
            "the artifact index {label} lists no version of {full_name} that fits this mur"
        );
        if let Some(newest) = artifact.versions.first() {
            let version = &newest.version;
            if let Some(note) = fit_note(full_name, version, &index_fit(newest, platform)) {
                let why = note
                    .strip_prefix(&format!("{full_name}: {version} "))
                    .unwrap_or(&note);
                message.push_str(&format!("; the newest it lists, {version}, {why}"));
            }
        }
        Err(no_version(message))
    }
}

/// `E-NEW-005` for `--index local`: the global store is not a published artifact index. Reads
/// nothing, so both forms of `mur new` call it before any config or network read.
pub(crate) fn refuse_global_store(index: Option<&str>) -> Result<(), CliError> {
    if index.map(IndexSource::parse) == Some(IndexSource::GlobalStore) {
        return Err(CliError::with_hint(
            E_NEW_005,
            "--index local names the global artifact store on this host, and mur new takes the \
             versions it writes from a published artifact index, which the store is not",
            "pass an index file or an http(s) URL, or omit --index to use the latest \
             default-artifacts release; `mur search <query> --index local` lists the global store",
        ));
    }
    Ok(())
}

/// The text of the index file at `path`, read on the host. `E-NEW-006` when it cannot be read.
pub(crate) fn read_index_text(path: &str) -> Result<String, CliError> {
    std::fs::read_to_string(path).map_err(|error| {
        CliError::with_hint(
            E_NEW_006,
            format!("could not read the artifact index file {path}: {error}"),
            INDEX_HINT,
        )
    })
}

/// `E-NEW-006` for the default source's unanswered index request.
fn source_unread(label: &str, error: &SourceError) -> CliError {
    let hint = match error {
        SourceError::RateLimited { .. } => format!("{}; or {INDEX_HINT}", unanswered_hint(error)),
        _ => INDEX_HINT.to_string(),
    };
    CliError::with_hint(
        E_NEW_006,
        format!(
            "could not read the artifact index of {label}: {}",
            error.reason()
        ),
        hint,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use murmur_artifact::{IndexedArtifact, IndexedAsset, IndexedVersion};

    use super::*;
    use crate::error::E_IDX_002;

    /// The committed verbatim copy of a default-artifacts release's index.
    fn committed_index() -> DefaultVersions {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/published-index/artifacts-index.json");
        DefaultVersions::load(Some(path.to_str().unwrap())).unwrap()
    }

    /// `murmur:stream/events@0.1.0` and `murmur:tool/run@0.2.0`, which this mur serves.
    const SERVED: [&str; 2] = ["murmur:stream/events@0.1.0", "murmur:tool/run@0.2.0"];

    /// [`SERVED`] and a package version this mur does not serve, sorted.
    const UNSERVED: [&str; 3] = [
        "murmur:driver/process@9.9.9",
        "murmur:stream/events@0.1.0",
        "murmur:tool/run@0.2.0",
    ];

    fn version(
        name: &str,
        version: &str,
        interfaces: &[&str],
        platform: Option<&str>,
    ) -> IndexedVersion {
        IndexedVersion {
            version: version.to_string(),
            release: "v1.0.0".to_string(),
            built_at_level: Some(1),
            interfaces: interfaces.iter().map(|i| i.to_string()).collect(),
            assets: vec![IndexedAsset {
                file: murmur_artifact::asset_file_name(name, version, platform),
                platform: platform.map(str::to_string),
                sha256: "0".repeat(64),
            }],
        }
    }

    /// An index of `scope` listing `name` at `versions`, newest first.
    fn index_of(scope: &str, name: &str, versions: Vec<IndexedVersion>) -> DefaultVersions {
        DefaultVersions {
            label: "test-index.json".to_string(),
            index: ArtifactIndex {
                schema_version: 2,
                scope: scope.to_string(),
                artifacts: vec![IndexedArtifact {
                    name: name.to_string(),
                    runtime: "driver".to_string(),
                    description: None,
                    tags: Vec::new(),
                    versions,
                }],
            },
        }
    }

    #[test]
    fn the_newest_fitting_version_beats_a_newer_one_that_does_not_fit() {
        let index = index_of(
            "murmur",
            "driver-anthropic",
            vec![
                version("driver-anthropic", "0.12.0", &UNSERVED, None),
                version("driver-anthropic", "0.11.0", &SERVED, None),
                version("driver-anthropic", "0.10.0", &SERVED, None),
            ],
        );
        assert_eq!(
            index.newest_fitting("murmur/driver-anthropic").unwrap(),
            "0.11.0"
        );
    }

    #[test]
    fn a_version_with_only_another_platforms_payload_is_skipped() {
        let index = index_of(
            "murmur",
            "tool-git",
            vec![
                version("tool-git", "0.3.0", &SERVED, Some("plan9-mips")),
                version("tool-git", "0.2.0", &SERVED, Some(current_platform())),
            ],
        );
        assert_eq!(index.newest_fitting("murmur/tool-git").unwrap(), "0.2.0");
    }

    #[test]
    fn an_unlisted_artifact_is_refused_naming_it_and_the_index() {
        let index = index_of(
            "murmur",
            "driver-openai",
            vec![version("driver-openai", "0.9.0", &SERVED, None)],
        );
        let error = index.newest_fitting("murmur/driver-anthropic").unwrap_err();
        assert_eq!(error.code, E_NEW_007);
        assert!(
            error.message.contains("murmur/driver-anthropic")
                && error.message.contains("test-index.json"),
            "{}",
            error.message
        );
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("--index")),
            "{error:?}"
        );
    }

    #[test]
    fn only_versions_that_do_not_fit_are_refused_naming_the_newest_and_why() {
        let index = index_of(
            "murmur",
            "driver-anthropic",
            vec![
                version("driver-anthropic", "0.12.0", &UNSERVED, None),
                version("driver-anthropic", "0.11.0", &UNSERVED, None),
            ],
        );
        let error = index.newest_fitting("murmur/driver-anthropic").unwrap_err();
        assert_eq!(error.code, E_NEW_007);
        for said in [
            "murmur/driver-anthropic",
            "test-index.json",
            "the newest it lists, 0.12.0, needs",
            "murmur:driver/process@9.9.9",
        ] {
            assert!(error.message.contains(said), "{said}: {}", error.message);
        }
    }

    #[test]
    fn an_index_of_another_scope_is_refused() {
        let index = index_of(
            "ana",
            "driver-anthropic",
            vec![version("driver-anthropic", "0.10.0", &SERVED, None)],
        );
        let error = index.newest_fitting("murmur/driver-anthropic").unwrap_err();
        assert_eq!(error.code, E_NEW_007);
        assert!(
            error.message.contains("'ana'") && error.message.contains("murmur/driver-anthropic"),
            "{}",
            error.message
        );
    }

    /// [`committed_index`] with every `murmur:tool/run` and `murmur:tool-registry/invoke`
    /// interface moved to the version WIT level 3 serves: the index a release built for level 3
    /// publishes.
    fn level_3_index() -> DefaultVersions {
        let committed = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/published-index/artifacts-index.json"),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts-index.json");
        std::fs::write(
            &path,
            committed
                .replace("murmur:tool/run@0.1.0", "murmur:tool/run@0.2.0")
                .replace(
                    "murmur:tool-registry/invoke@0.1.0",
                    "murmur:tool-registry/invoke@0.2.0",
                ),
        )
        .unwrap();
        DefaultVersions::load(Some(path.to_str().unwrap())).unwrap()
    }

    /// The committed release was built for WIT level 2, whose `murmur:tool` this mur no longer
    /// serves: a WASM tool or driver it lists fits nothing until a level-3 release is published,
    /// and the refusal names the package.
    #[test]
    fn the_committed_level_2_index_lists_no_wasm_artifact_this_mur_fits() {
        let index = committed_index();
        for full_name in ["murmur/tool-editor", "murmur/driver-anthropic"] {
            let error = index.newest_fitting(full_name).unwrap_err();
            assert_eq!(error.code, E_NEW_007, "{full_name}");
            assert!(
                error.message.contains("this mur serves murmur:tool@0.2.0"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn the_committed_index_names_the_versions_it_lists() {
        let index = level_3_index();
        for (full_name, expected) in [
            ("murmur/tool-registry-search", "0.2.0"),
            ("murmur/tool-editor", "0.6.0"),
            ("murmur/skill-create-manifest", "0.1.0"),
            ("murmur/driver-anthropic", "0.10.0"),
            ("murmur/driver-openai", "0.9.0"),
        ] {
            assert_eq!(
                index.newest_fitting(full_name).unwrap(),
                expected,
                "{full_name}"
            );
        }
        assert!(
            index.label().ends_with("artifacts-index.json"),
            "{}",
            index.label()
        );
    }

    #[test]
    fn local_is_refused_before_anything_is_read() {
        let error = DefaultVersions::load(Some("local")).unwrap_err();
        assert_eq!(error.code, E_NEW_005);
        let hint = error.hint.unwrap();
        assert!(hint.contains("omit --index"), "{hint}");
        assert!(hint.contains("mur search <query> --index local"), "{hint}");
    }

    #[test]
    fn an_unreadable_file_or_url_is_e_new_006_and_a_bad_file_e_idx_002() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let error = DefaultVersions::load(Some(missing.to_str().unwrap())).unwrap_err();
        assert_eq!(error.code, E_NEW_006);
        assert!(error.message.contains("missing.json"), "{}", error.message);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("--index <PATH|URL>")),
            "{error:?}"
        );

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/artifacts-index.json",
            closed.local_addr().unwrap()
        );
        drop(closed);
        let error = DefaultVersions::load(Some(&url)).unwrap_err();
        assert_eq!(error.code, E_NEW_006);
        assert!(error.message.contains(&url), "{}", error.message);

        let old = dir.path().join("old.json");
        std::fs::write(&old, r#"{"schema_version":"1","artifacts":[]}"#).unwrap();
        let error = DefaultVersions::load(Some(old.to_str().unwrap())).unwrap_err();
        assert_eq!(error.code, E_IDX_002);
    }

    #[test]
    fn the_default_source_is_labelled_with_its_release_and_held_to_its_scope() {
        use crate::source::memory::MemorySource;
        use crate::source::DEFAULT_ARTIFACTS_REPO;

        let mut source = MemorySource::new(DEFAULT_ARTIFACTS_REPO, MURMUR_SCOPE, 1, "v0.99.0");
        source.publish(
            "v0.99.0",
            "driver-anthropic",
            "0.11.0",
            None,
            &SERVED,
            vec![1],
        );
        source.publish(
            "v0.99.0",
            "driver-anthropic",
            "0.12.0",
            None,
            &UNSERVED,
            vec![2],
        );
        let versions = DefaultVersions::from_source(&source).unwrap();
        assert_eq!(
            versions.label(),
            "github:murmur-nexus/default-artifacts v0.99.0"
        );
        assert_eq!(
            versions.newest_fitting("murmur/driver-anthropic").unwrap(),
            "0.11.0"
        );
        assert!(source.fetched().is_empty(), "no payload is fetched");

        let mut foreign = MemorySource::new(DEFAULT_ARTIFACTS_REPO, MURMUR_SCOPE, 1, "v0.99.0");
        foreign.index.scope = "ana".to_string();
        let error = DefaultVersions::from_source(&foreign).unwrap_err();
        assert_eq!(error.code, E_NEW_007);
        assert!(error.message.contains("'ana'"), "{}", error.message);

        let mut unread = MemorySource::new(DEFAULT_ARTIFACTS_REPO, MURMUR_SCOPE, 1, "v0.99.0");
        unread.index_error = Some(SourceError::Other("connection refused".to_string()));
        let error = DefaultVersions::from_source(&unread).unwrap_err();
        assert_eq!(error.code, E_NEW_006);
        assert_eq!(
            error.message,
            "could not read the artifact index of github:murmur-nexus/default-artifacts: \
             connection refused"
        );
        assert_eq!(error.hint.as_deref(), Some(INDEX_HINT));
    }

    #[test]
    fn a_rate_limited_source_carries_the_token_advice_and_names_index() {
        let error = source_unread(
            "github:murmur-nexus/default-artifacts",
            &SourceError::RateLimited {
                status: 403,
                message: "API rate limit exceeded".to_string(),
                signal: crate::source::RateLimitSignal::RemainingHeader,
                resets_in_secs: Some(600),
                authenticated: false,
            },
        );
        assert_eq!(error.code, E_NEW_006);
        assert!(
            error
                .message
                .starts_with("could not read the artifact index of github:murmur-nexus/default-artifacts: rate limited"),
            "{}",
            error.message
        );
        let hint = error.hint.unwrap();
        assert!(
            hint.contains("GITHUB_TOKEN") && hint.contains("--index <PATH|URL>"),
            "{hint}"
        );
    }
}
