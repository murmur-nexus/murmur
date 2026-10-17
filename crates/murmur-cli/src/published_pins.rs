//! Every artifact version `mur` or its docs tell a user to install is one the default artifacts'
//! published index lists. The index is the committed copy under
//! `tests/fixtures/published-index/`, whose `README.md` says which release it is.

use std::{fs, path::Path};

use murmur_artifact::{ArtifactIndex, ArtifactName, RuntimeManifest};

use crate::commands::new_roster::PROVIDERS;

const PUBLISHED_INDEX: &str = "tests/fixtures/published-index/artifacts-index.json";

fn repo_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn published_index() -> ArtifactIndex {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PUBLISHED_INDEX);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let index = ArtifactIndex::from_json(&text)
        .unwrap_or_else(|error| panic!("{PUBLISHED_INDEX} does not parse: {error}"));
    assert_eq!(
        index.scope, "murmur",
        "{PUBLISHED_INDEX} is not the murmur scope's index"
    );
    index
}

/// Panics, naming `origin` and `full_name@version`, unless the published index lists that
/// version of that artifact.
fn assert_published(index: &ArtifactIndex, origin: &str, full_name: &str, version: &str) {
    let name = ArtifactName::parse(full_name)
        .unwrap_or_else(|error| panic!("{origin} pins {full_name}@{version}: {error}"));
    assert_eq!(
        name.scope(),
        index.scope,
        "{origin} pins {full_name}@{version}, outside the {} scope the published index serves",
        index.scope
    );
    let listed = index
        .artifacts
        .iter()
        .find(|artifact| artifact.name == name.name())
        .unwrap_or_else(|| {
            panic!(
                "{origin} pins {full_name}@{version}, and {PUBLISHED_INDEX} lists no {full_name}"
            )
        });
    let versions: Vec<&str> = listed
        .versions
        .iter()
        .map(|entry| entry.version.as_str())
        .collect();
    assert!(
        versions.contains(&version),
        "{origin} pins {full_name}@{version}, a version {PUBLISHED_INDEX} does not list; it lists {}",
        versions.join(", ")
    );
}

/// The `extra.v` entries of `docs/mkdocs.yml` that pin a default artifact, as `(key, version)`.
fn docs_artifact_pins(mkdocs: &str) -> Vec<(String, String)> {
    let mut in_v = false;
    let mut pins = Vec::new();
    for line in mkdocs.lines() {
        if line == "  v:" {
            in_v = true;
            continue;
        }
        if in_v && !line.is_empty() && !line.starts_with("    ") {
            break;
        }
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        if in_v && key.starts_with("murmur_") {
            pins.push((key.to_string(), value.trim().trim_matches('"').to_string()));
        }
    }
    pins
}

/// The `extra.v.<key>` pin `docs/mkdocs.yml` holds, such as `murmur_tool_editor`'s.
pub(crate) fn docs_pin(key: &str) -> String {
    docs_artifact_pins(&repo_file("docs/mkdocs.yml"))
        .into_iter()
        .find_map(|(pinned, version)| (pinned == key).then_some(version))
        .unwrap_or_else(|| panic!("docs/mkdocs.yml has no extra.v.{key}"))
}

/// `murmur_tool_editor` → `murmur/tool-editor`.
fn docs_key_artifact(key: &str) -> String {
    let rest = key.strip_prefix("murmur_").expect("a murmur_ key");
    format!("murmur/{}", rest.replace('_', "-"))
}

/// The first `murmur.yaml` block of `README.md`: the quickstart.
fn readme_quickstart(readme: &str) -> String {
    let start = readme
        .find("`murmur.yaml`\n```yaml\n")
        .expect("README.md has no `murmur.yaml` yaml block")
        + "`murmur.yaml`\n```yaml\n".len();
    let length = readme[start..]
        .find("```")
        .expect("README.md's murmur.yaml block is not closed");
    readme[start..start + length].to_string()
}

fn assert_manifest_published(index: &ArtifactIndex, origin: &str, yaml: &str) {
    let manifest = RuntimeManifest::from_yaml_str(yaml)
        .unwrap_or_else(|error| panic!("{origin} does not parse as a manifest: {error}"));
    assert!(
        !manifest.artifacts.is_empty(),
        "{origin} declares no artifact"
    );
    for artifact in &manifest.artifacts {
        assert_published(index, origin, &artifact.name, &artifact.version);
    }
}

#[test]
fn published_pins_roster_drivers() {
    let index = published_index();
    for provider in PROVIDERS {
        assert_published(
            &index,
            &format!("mur new --roster ({} provider)", provider.name),
            provider.driver,
            provider.driver_version,
        );
    }
}

#[test]
fn published_pins_docs_extra_v() {
    let index = published_index();
    let mkdocs = repo_file("docs/mkdocs.yml");
    let pins = docs_artifact_pins(&mkdocs);
    assert!(!pins.is_empty(), "docs/mkdocs.yml extra.v pins no artifact");
    for (key, version) in pins {
        assert_published(
            &index,
            &format!("docs/mkdocs.yml extra.v.{key}"),
            &docs_key_artifact(&key),
            &version,
        );
    }
}

#[test]
fn published_pins_readme_quickstart() {
    let index = published_index();
    let readme = repo_file("README.md");
    assert_manifest_published(
        &index,
        "README.md's quickstart murmur.yaml",
        &readme_quickstart(&readme),
    );
}

#[cfg(feature = "beta-mur-new")]
#[test]
fn published_pins_beta_generator() {
    use crate::{commands::new::build_meta_manifest, config::InferenceConfig};

    let index = published_index();
    for provider in PROVIDERS {
        let config = InferenceConfig {
            provider: provider.name.to_string(),
            model: provider.default_model.to_string(),
            api_key: String::new(),
            endpoint: String::new(),
        };
        assert_manifest_published(
            &index,
            &format!(
                "mur new <task>'s generator manifest ({} provider)",
                provider.name
            ),
            &build_meta_manifest(&config, &[]),
        );
    }
}
