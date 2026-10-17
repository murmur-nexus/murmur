use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use murmur_artifact::{
    current_platform, resolve_manifest_path, ArtifactRuntime, LocalRegistry, Registry,
    RegistryError, RuntimeManifest, ARTIFACT_INDEX_FILE_NAME, MANIFEST_FILENAME,
};

use super::new_roster::{Provider, ANTHROPIC, OPENAI};
use super::search::{read_index_file_verbatim, IndexSource, DEFAULT_INDEX_URL};
use crate::{
    config::{load_mur_config, save_mur_config, InferenceConfig},
    error::{CliError, E_CFG_001, E_IO_003, E_MAN_002, E_NEW_005, E_RUN_008},
};

// Temporary directory that cleans itself up on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn create() -> Result<Self, CliError> {
        let id = uuid::Uuid::new_v4();
        let path = std::path::PathBuf::from("/tmp").join(format!("murmur-new-{id}"));
        fs::create_dir_all(&path).map_err(|e| {
            CliError::new(E_IO_003, format!("failed to create temp directory: {e}"))
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var("MUR_KEEP_SESSION").is_ok() {
            capsule_runtime::report_eprintln!(
                "mur new: session dir preserved at {}",
                self.path.display()
            );
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// The `gateway.api_key` [`build_meta_manifest`] writes on the driver entry. A reference, so the
/// entry parses as keyed; [`inject_driver_key`] replaces it before staging, so the variable is
/// never read.
const META_MANIFEST_KEY_PLACEHOLDER: &str = "${MURMUR_NEW_API_KEY}";

/// Replaces the configured driver's `gateway.api_key` with `key` as a literal, in the parsed
/// manifest only.
fn inject_driver_key(runtime_manifest: &mut RuntimeManifest, key: &str) {
    let driver_name = runtime_manifest
        .inference
        .as_ref()
        .and_then(|inference| inference.driver.as_ref())
        .map(|driver| driver.artifact.clone());
    if let Some(gateway) = runtime_manifest
        .artifacts
        .iter_mut()
        .find(|artifact| Some(&artifact.name) == driver_name.as_ref())
        .and_then(|artifact| artifact.gateway.as_mut())
    {
        gateway.api_key = Some(murmur_artifact::ApiKeyReference::Literal(key.to_string()));
    }
}

/// The origins `murmur/tool-registry-search` reads the index on the latest default-artifacts
/// release from: the release download URL, and the asset host it redirects to. A native tool
/// runs under the capsule-wide grants, so the generator capsule lists both itself.
pub(crate) const REGISTRY_SEARCH_ORIGINS: [&str; 2] = [
    "https://github.com",
    "https://release-assets.githubusercontent.com",
];

/// The index `--index` names, as the generator capsule's search tool reads it.
#[derive(Debug, PartialEq, Eq)]
enum GeneratorIndex {
    /// No `--index`: the index on the latest default-artifacts release, the tool's own default.
    Default,
    /// An `http(s)` URL the tool fetches, and its `<scheme>://<host>[:<port>]` origin.
    Url { url: String, origin: String },
    /// An index file checked on the host, as read. The tool reads a copy of it in its work
    /// folder, [`ARTIFACT_INDEX_FILE_NAME`]: a host path outside that folder is unreadable to it.
    File { text: String },
}

impl GeneratorIndex {
    /// Errors before anything is staged: `E-NEW-005` for `local`, the global store the capsule
    /// cannot read; `E-IO-003` for an unparseable URL or an unreadable file; `E-IDX-002` for a
    /// file that is not a schema 2 index.
    fn resolve(index: Option<&str>) -> Result<Self, CliError> {
        let Some(index) = index else {
            return Ok(Self::Default);
        };
        match IndexSource::parse(index) {
            IndexSource::GlobalStore => Err(CliError::with_hint(
                E_NEW_005,
                "--index local names the global artifact store on this host, which the \
                 generator capsule cannot read",
                "run `mur search <query> --index local` to list it, or pass an index file or an \
                 http(s) URL",
            )),
            IndexSource::Url(url) => {
                let origin = url::Url::parse(url)
                    .map(|parsed| parsed.origin())
                    .ok()
                    .filter(url::Origin::is_tuple)
                    .ok_or_else(|| {
                        CliError::new(
                            E_IO_003,
                            format!("artifact index URL {url} has no scheme, host and port"),
                        )
                    })?;
                Ok(Self::Url {
                    url: url.to_string(),
                    origin: origin.ascii_serialization(),
                })
            }
            IndexSource::File(path) => {
                let (_, text) = read_index_file_verbatim(path)?;
                Ok(Self::File { text })
            }
        }
    }

    /// The search tool's `index` input. The default index is named rather than left out: a
    /// model given a strict function schema, as OpenAI's Responses API makes every tool's, fills
    /// every property, and the tool reads an empty `index` as a file path.
    fn tool_input(&self) -> &str {
        match self {
            Self::Default => DEFAULT_INDEX_URL,
            Self::Url { url, .. } => url,
            Self::File { .. } => ARTIFACT_INDEX_FILE_NAME,
        }
    }

    /// The origin the generator's network allow list needs beyond [`REGISTRY_SEARCH_ORIGINS`].
    fn extra_network_allow(&self) -> Vec<String> {
        match self {
            Self::Url { origin, .. } if !REGISTRY_SEARCH_ORIGINS.contains(&origin.as_str()) => {
                vec![origin.clone()]
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) fn run_new(task: &str, index: Option<&str>) -> Result<(), CliError> {
    let index = GeneratorIndex::resolve(index)?;

    // Resolve inference config: config file → env var → interactive wizard.
    let inf = resolve_inference_config()?;

    let local_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    // Build the dynamic meta-manifest with a placeholder key reference — parsing never
    // touches the environment — then inject the real key directly into the parsed struct.
    // This keeps the resolved key a Rust `String` in memory only; it is never round-tripped
    // through this process's own environment (which a /proc/<pid>/environ read could expose).
    let meta_yaml = build_meta_manifest(&inf, &index.extra_network_allow());
    let mut runtime_manifest = RuntimeManifest::from_yaml_str(&meta_yaml).map_err(|e| {
        CliError::new(
            E_MAN_002,
            format!("internal: generator meta-manifest is invalid: {e}"),
        )
    })?;
    inject_driver_key(&mut runtime_manifest, &inf.api_key);

    // Check that all generator capsule artifacts are installed before staging.
    let generator_artifacts: Vec<ArtifactRequest> = runtime_manifest
        .artifacts
        .iter()
        .map(|a| ArtifactRequest {
            name: a.name.clone(),
            version: a.version.clone(),
            runtime: a.runtime.clone(),
            source: a.source.clone(),
            on_overflow: a.on_overflow,
            config: a.config.clone(),
            gateway: a.gateway.clone(),
            capabilities: a.capabilities.clone(),
        })
        .collect();

    check_artifacts_installed(&local_registry, &generator_artifacts)?;

    // Build the task prompt with a compact inline schema.
    let task_prompt = build_task_prompt(task, index.tool_input(), &inf);

    // Create a temp dir for the generator session. The session workdir lives inside it
    // (manifest_dir/workdir/<session_id>/), so cleanup on Drop removes everything.
    let temp_dir = TempDir::create()?;
    let manifest_dir = temp_dir.path().to_path_buf();

    // stage_session expects manifest_dir/murmur.yaml for system_prompt_file resolution.
    fs::write(manifest_dir.join(MANIFEST_FILENAME), &meta_yaml)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to write generator manifest: {e}")))?;

    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);
    let source_supply = crate::source::supply::CliSourceSupply::for_policy(&capability_policy)?;
    let mut allowlisted_tools = HashSet::new();
    for artifact in &runtime_manifest.artifacts {
        if artifact.runtime == ArtifactRuntime::Tool {
            allowlisted_tools.insert(artifact.name.clone());
        }
    }

    let stage_request = StageRequest {
        credentials_file: None,
        manifest_dir,
        capsule_name: runtime_manifest.name.clone(),
        capsule_version: runtime_manifest.version.clone(),
        capsule_artifact: None,
        capsule_component_bytes: Vec::new(), // agent capsule — no WASM component
        artifacts: generator_artifacts,
        allowlisted_tools,
        lock_expectations: None, // ephemeral generator; no lockfile
        capability_policy,
        inference: runtime_manifest.inference.clone(),
        system_prompt_overridden: false,
        context: runtime_manifest.context.clone(),
        context_id: None,
        resume: None,
        forget_session: false,
        otel_endpoint: None,
        eval_config_json: None,
        case_id: None,
        dataset_id: None,
        lifecycle: runtime_manifest.lifecycle.clone(),
        lifecycle_override: None,
        trace: None,
        workdir: None, // let runtime create session dir inside manifest_dir/workdir/
        bind_addr: "127.0.0.1".to_string(),
        internal_port: None,
        // Ephemeral generator capsule: it declares no containment, so this is the default
        // floor every host clears rather than an inherited one.
        declared_containment_floor: runtime_manifest
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.containment)
            .unwrap_or_default(),
        exports: runtime_manifest.exports.clone(),
        control: None,
        door_authentication: None,
        spawn_grant: None,
        machine_tokens_per_day: None,
        formation_id: None,
        formation_member: None,
        source_supply,
    };

    // Stage the session (creates workdir, installs artifacts including skill.md).
    let staged = stage_session(Arc::new(local_registry), stage_request)
        .map_err(crate::wit_level::staging_error)?;
    let session_root = staged.session_root.clone();

    // Write the task prompt as task.md. The runtime reads this as the initial task when
    // task_acceptance: single and task.md exists — no A2A handshake needed.
    staged
        .write_task_input(task_prompt.as_bytes())
        .map_err(|e| CliError::new(E_IO_003, format!("failed to write task prompt: {e}")))?;
    if let GeneratorIndex::File { text } = &index {
        staged
            .write_workdir_file(ARTIFACT_INDEX_FILE_NAME, text.as_bytes())
            .map_err(|e| {
                CliError::new(
                    E_IO_003,
                    format!("failed to copy the artifact index into the generator's workdir: {e}"),
                )
            })?;
    }

    capsule_runtime::report_eprintln!("mur new: generating manifest...");

    // Run the generator synchronously. The capsule reads skill.md via murmur/tool-editor,
    // discovers artifacts via murmur/tool-registry-search, generates the manifest, and writes
    // it to out/murmur.yaml. The session exits via after_task: exit.
    launch_session(staged, |_url| {}).map_err(CliError::from)?;

    // Read the manifest written by the agent via write_file("out/murmur.yaml").
    // Read through the session root, so a link the generator left there is refused rather than
    // followed.
    let manifest_yaml = match session_root.read_to_string("out/murmur.yaml") {
        Ok(Some(manifest_yaml)) => manifest_yaml,
        Ok(None) => {
            let agent_output = session_root
                .read_to_string("out/result.txt")
                .ok()
                .flatten()
                .unwrap_or_else(|| "(no output captured)".to_string());
            return Err(CliError::new(
                "E-NEW-001",
                format!(
                    "generator agent did not produce out/murmur.yaml\nagent output:\n{agent_output}"
                ),
            ));
        }
        Err(e) => {
            return Err(CliError::new(
                E_IO_003,
                format!("failed to read out/murmur.yaml: {e}"),
            ))
        }
    };

    // Validate structure before touching CWD.
    if std::env::var("MUR_DEBUG_MANIFEST").is_ok() {
        capsule_runtime::report_eprintln!(
            "--- generated manifest ---\n{manifest_yaml}\n--- end ---"
        );
    }
    RuntimeManifest::from_yaml_str(&manifest_yaml).map_err(|e| {
        CliError::new(
            E_MAN_002,
            format!("generated manifest failed validation: {e}"),
        )
    })?;

    // Write atomically: write to a temp file in CWD then rename into place.
    // This prevents a partial manifest if the process is interrupted mid-write.
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current directory: {e}"),
        )
    })?;
    let output_path = resolve_manifest_path(&cwd);
    let tmp_path = cwd.join(format!(".{MANIFEST_FILENAME}.tmp"));
    fs::write(&tmp_path, &manifest_yaml).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to write {}: {e}", tmp_path.display()),
        )
    })?;
    fs::rename(&tmp_path, &output_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        CliError::new(
            E_IO_003,
            format!("failed to rename to {}: {e}", output_path.display()),
        )
    })?;

    capsule_runtime::report_println!("{MANIFEST_FILENAME} written to {}", output_path.display());
    Ok(())
}

fn resolve_inference_config() -> Result<InferenceConfig, CliError> {
    // 1. Config file takes first precedence.
    let cfg = load_mur_config()?;
    if let Some(inf) = cfg.inference {
        if inf.is_complete() {
            return Ok(inf);
        }
    }

    // 2. Env var fallback.
    for provider in [&ANTHROPIC, &OPENAI] {
        if let Ok(key) = std::env::var(provider.key_var) {
            if !key.is_empty() {
                capsule_runtime::report_eprintln!(
                    "hint: found {} in environment; add [inference] to ~/.murmur/config.yaml to persist your provider settings",
                    provider.key_var
                );
                return Ok(InferenceConfig {
                    provider: provider.name.to_string(),
                    model: provider.default_model.to_string(),
                    api_key: key,
                    endpoint: String::new(),
                });
            }
        }
    }

    // 3. Interactive wizard.
    run_wizard()
}

fn run_wizard() -> Result<InferenceConfig, CliError> {
    let non_tty_err = || {
        CliError::with_hint(
            E_CFG_001,
            "no inference provider configured and wizard cannot run in non-interactive mode",
            "set ANTHROPIC_API_KEY or add [inference] to ~/.murmur/config.yaml",
        )
    };

    capsule_runtime::report_eprintln!("No inference provider configured.");

    let theme = ColorfulTheme::default();

    let provider_idx = Select::with_theme(&theme)
        .with_prompt("Provider")
        .items(["Anthropic", "OpenAI"])
        .default(0)
        .interact()
        .map_err(|_| non_tty_err())?;

    // The first model each provider offers is its default, labelled as the recommendation.
    let (provider, model_values): (&Provider, &[&str]) = match provider_idx {
        0 => (
            &ANTHROPIC,
            &[
                ANTHROPIC.default_model,
                "claude-sonnet-4-5-20251001",
                "claude-opus-4-5-20251001",
            ],
        ),
        _ => (&OPENAI, &[OPENAI.default_model, "gpt-4o", "o3-mini"]),
    };
    let mut model_labels: Vec<String> =
        model_values.iter().map(|model| model.to_string()).collect();
    model_labels[0].push_str("  (fast — recommended)");
    model_labels.push("Enter manually".to_string());

    let model_idx = Select::with_theme(&theme)
        .with_prompt("Model")
        .items(&model_labels)
        .default(0)
        .interact()
        .map_err(|_| non_tty_err())?;

    let model = if model_idx == model_labels.len() - 1 {
        Input::<String>::with_theme(&theme)
            .with_prompt("Model name")
            .interact_text()
            .map_err(|_| non_tty_err())?
    } else {
        model_values[model_idx].to_string()
    };

    let api_key = Password::with_theme(&theme)
        .with_prompt("API key")
        .interact()
        .map_err(|_| non_tty_err())?;

    let config = InferenceConfig {
        provider: provider.name.to_string(),
        model,
        api_key,
        endpoint: String::new(),
    };

    let save = Confirm::with_theme(&theme)
        .with_prompt("Save to ~/.murmur/config.yaml?")
        .default(true)
        .interact()
        .unwrap_or(false);

    if save {
        let mut mur_config = load_mur_config()?;
        mur_config.inference = Some(config.clone());
        save_mur_config(&mur_config)?;
    }

    Ok(config)
}

/// A tool or skill the generator capsule declares besides its driver.
pub(crate) struct GeneratorArtifact {
    pub(crate) name: &'static str,
    /// The `extra.v` value `docs/mkdocs.yml` holds for this artifact (`murmur/tool-editor` →
    /// `murmur_tool_editor`), which a unit test keeps equal.
    pub(crate) version: &'static str,
    pub(crate) runtime: &'static str,
}

/// The generator capsule's tools and skill, in the order its manifest declares them. Its driver
/// is the provider's, at [`Provider::driver_version`].
pub(crate) const GENERATOR_ARTIFACTS: [GeneratorArtifact; 3] = [
    GeneratorArtifact {
        name: "murmur/tool-registry-search",
        version: "0.2.0",
        runtime: "tool",
    },
    GeneratorArtifact {
        name: "murmur/tool-editor",
        version: "0.6.0",
        runtime: "tool",
    },
    GeneratorArtifact {
        name: "murmur/skill-create-manifest",
        version: "0.1.0",
        runtime: "skill",
    },
];

/// The generator capsule's manifest. Its `capabilities.network.allow` lists
/// [`REGISTRY_SEARCH_ORIGINS`], then `extra_network_allow`.
pub(crate) fn build_meta_manifest(
    config: &InferenceConfig,
    extra_network_allow: &[String],
) -> String {
    let provider: &Provider = match config.provider.as_str() {
        "openai" => &OPENAI,
        _ => &ANTHROPIC,
    };
    let endpoint = if config.endpoint.is_empty() {
        provider.default_endpoint
    } else {
        config.endpoint.as_str()
    };
    let others: String = GENERATOR_ARTIFACTS
        .iter()
        .map(|artifact| {
            format!(
                "  - name: {}\n    version: \"{}\"\n    runtime: {}\n",
                artifact.name, artifact.version, artifact.runtime
            )
        })
        .collect();
    let network_allow: String = REGISTRY_SEARCH_ORIGINS
        .iter()
        .copied()
        .chain(extra_network_allow.iter().map(String::as_str))
        .map(|origin| format!("      - \"{origin}\"\n"))
        .collect();

    format!(
        r#"name: murmur-manifest-generator
version: "0.4.17"
artifacts:
  - name: {driver_name}
    version: "{driver_version}"
    runtime: driver
    gateway:
      endpoint: {endpoint}
      api_key: {META_MANIFEST_KEY_PLACEHOLDER}
{others}capabilities:
  network:
    allow:
{network_allow}lifecycle:
  task_acceptance: single
  after_task: exit
inference:
  model: {model}
  driver:
    artifact: {driver_name}
"#,
        driver_name = provider.driver,
        driver_version = provider.driver_version,
        model = config.model,
    )
}

/// `E-RUN-008` for the first generator artifact the global store does not hold. The generator
/// stages from the global store, so the hint installs there: a project install never reaches it.
fn check_artifacts_installed(
    local_registry: &LocalRegistry,
    artifacts: &[ArtifactRequest],
) -> Result<(), CliError> {
    for artifact in artifacts {
        let name = murmur_artifact::ArtifactName::parse(&artifact.name)
            .map_err(|error| CliError::new(E_RUN_008, error.to_string()))?;
        match local_registry.resolve_with_platform(
            &name,
            &artifact.version,
            Some(current_platform()),
        ) {
            Ok(_) => {}
            Err(RegistryError::NotFound { .. }) => {
                return Err(CliError::with_hint(
                    E_RUN_008,
                    format!("generator artifact '{}' is not installed", artifact.name),
                    format!(
                        "run `mur install -g {}@{}` to install it",
                        artifact.name, artifact.version
                    ),
                ));
            }
            Err(error) => return Err(CliError::from(error)),
        }
    }
    Ok(())
}

/// The prompt for the generator capsule. `index` is the registry-search tool's `index` input,
/// shown in the example call.
fn build_task_prompt(task: &str, index: &str, _inf: &InferenceConfig) -> String {
    format!(
        r#"Step 1 — Read your manifest generation guide:
Call read_file with path "tools/skill-create-manifest/skill.md".
Read the full contents before proceeding.

Step 2 — Search for available artifacts:
Call the registry search tool with a query relevant to the task.
Example: {{"query": "git editor shell", "index": {index}}}
Pass this index on every search call.
Each result names an artifact by its full <scope>/<name> and lists its versions newest first.
Use each artifact's full <scope>/<name>, at the newest version the results list for it.

Step 3 — Generate the manifest:
Task: {task}

Follow all guidance from the skill file. The manifest must pass `mur build` validation.
Prefer murmur/tool-git and murmur/tool-editor over granting bash shell access.
Name every artifact by its full <scope>/<name>, at the newest version the registry search lists.
Include runtime: tool at the top level.

Step 4 — Write the manifest:
Call write_file with path "out/murmur.yaml" and the YAML content.
Do not include markdown fences in the file content — write raw YAML only.
Output only the word DONE when finished.
"#,
        task = task,
        index = serde_json::Value::from(index),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build key-shaped test values at runtime so the source never contains a
    /// credential-shaped literal that secret scanners could flag.
    fn fake_key(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn build_meta_manifest_anthropic_contains_correct_driver() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config, &[]);
        assert!(
            yaml.contains("name: murmur/driver-anthropic"),
            "should use anthropic driver"
        );
        assert!(
            yaml.contains("api.anthropic.com"),
            "should use anthropic endpoint"
        );
        assert!(
            yaml.contains("claude-haiku-4-5-20251001"),
            "should use model"
        );
        assert!(
            !yaml.contains("murmur/driver-openai"),
            "should not reference openai driver"
        );
    }

    #[test]
    fn build_meta_manifest_openai_contains_correct_driver() {
        let config = InferenceConfig {
            provider: "openai".to_string(),
            model: "gpt-5.6-luna".to_string(),
            api_key: fake_key(&["sk-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config, &[]);
        assert!(
            yaml.contains("name: murmur/driver-openai"),
            "should use openai driver"
        );
        assert!(
            yaml.contains("api.openai.com"),
            "should use openai endpoint"
        );
        assert!(yaml.contains("gpt-5.6-luna"), "should use model");
        assert!(
            !yaml.contains("murmur/driver-anthropic"),
            "should not reference anthropic driver"
        );
    }

    #[test]
    fn build_meta_manifest_uses_custom_endpoint_when_set() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: "https://custom.proxy.example.com".to_string(),
        };
        let yaml = build_meta_manifest(&config, &[]);
        assert!(
            yaml.contains("custom.proxy.example.com"),
            "should use custom endpoint"
        );
        assert!(
            !yaml.contains("api.anthropic.com"),
            "should not use default endpoint"
        );
    }

    #[test]
    fn build_meta_manifest_uses_a_key_placeholder() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config, &[]);
        assert!(
            yaml.contains(&format!("api_key: {META_MANIFEST_KEY_PLACEHOLDER}")),
            "the driver entry must carry the placeholder key reference"
        );
        assert_eq!(
            yaml.matches("${").count(),
            1,
            "the placeholder must be the manifest's only env-reference token"
        );
        assert!(
            !yaml.contains("MUR_INFERENCE_API_KEY"),
            "must not reference MUR_INFERENCE_API_KEY"
        );
        assert!(
            !yaml.contains(&config.api_key),
            "must not embed the raw api key literal"
        );
        assert!(
            !yaml.contains("ANTHROPIC_API_KEY"),
            "must not reference ANTHROPIC_API_KEY"
        );
        assert!(
            !yaml.contains("OPENAI_API_KEY"),
            "must not reference OPENAI_API_KEY"
        );
    }

    #[test]
    fn build_meta_manifest_parses_without_any_env_var_present() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_mur = std::env::var("MUR_INFERENCE_API_KEY").ok();
        unsafe { std::env::remove_var("MUR_INFERENCE_API_KEY") };

        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config, &[]);
        let runtime_manifest = RuntimeManifest::from_yaml_str(&yaml)
            .expect("build_meta_manifest should produce a valid runtime manifest without any env var present");
        let driver = runtime_manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.name == "murmur/driver-anthropic")
            .expect("the driver entry is declared");
        assert_eq!(
            driver
                .gateway
                .as_ref()
                .map(|gateway| gateway.endpoint.as_str()),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            driver
                .gateway
                .as_ref()
                .and_then(|gateway| gateway.api_key.clone()),
            Some(murmur_artifact::ApiKeyReference::Environment(
                "MURMUR_NEW_API_KEY".to_string()
            )),
            "parsed gateway.api_key holds the placeholder until the caller injects the real key"
        );
        assert!(runtime_manifest.inference.is_some());

        unsafe {
            match &saved_mur {
                Some(v) => std::env::set_var("MUR_INFERENCE_API_KEY", v),
                None => std::env::remove_var("MUR_INFERENCE_API_KEY"),
            }
        }
    }

    #[test]
    fn mur_inference_api_key_env_var_is_never_set_by_generator_flow() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_ant = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        let saved_mur = std::env::var("MUR_INFERENCE_API_KEY").ok();
        unsafe {
            std::env::set_var(
                "ANTHROPIC_API_KEY",
                fake_key(&["sk-", "ant-", "regression-test"]),
            );
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("MUR_INFERENCE_API_KEY");
        }

        // Mirrors run_new's generator path: resolve config → build meta-manifest →
        // parse → inject the real key into the parsed struct in memory.
        let result = (|| -> Result<RuntimeManifest, CliError> {
            let inf = resolve_inference_config()?;
            let meta_yaml = build_meta_manifest(&inf, &[]);
            let mut runtime_manifest = RuntimeManifest::from_yaml_str(&meta_yaml).map_err(|e| {
                CliError::new(
                    E_MAN_002,
                    format!("internal: generator meta-manifest is invalid: {e}"),
                )
            })?;
            inject_driver_key(&mut runtime_manifest, &inf.api_key);
            Ok(runtime_manifest)
        })();

        let mur_var_after = std::env::var("MUR_INFERENCE_API_KEY");

        unsafe {
            match &saved_ant {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
            match &saved_oai {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
            match &saved_mur {
                Some(v) => std::env::set_var("MUR_INFERENCE_API_KEY", v),
                None => std::env::remove_var("MUR_INFERENCE_API_KEY"),
            }
        }

        assert!(
            mur_var_after.is_err(),
            "MUR_INFERENCE_API_KEY must never be set on the process by the generator flow"
        );

        let runtime_manifest =
            result.expect("generator flow should succeed with ANTHROPIC_API_KEY set");
        let driver_key = runtime_manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.name == "murmur/driver-anthropic")
            .and_then(|artifact| artifact.gateway.as_ref())
            .and_then(|gateway| gateway.api_key.clone());
        assert_eq!(
            driver_key,
            Some(murmur_artifact::ApiKeyReference::Literal(fake_key(&[
                "sk-",
                "ant-",
                "regression-test"
            ]))),
            "the driver entry's gateway.api_key must hold the real key in memory"
        );
    }

    #[test]
    fn inference_config_is_complete_returns_false_when_api_key_empty() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: String::new(),
            endpoint: String::new(),
        };
        assert!(!config.is_complete());
    }

    #[test]
    fn inference_config_is_complete_returns_false_when_provider_empty() {
        let config = InferenceConfig {
            provider: String::new(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        assert!(!config.is_complete());
    }

    #[test]
    fn inference_config_is_complete_returns_true_when_all_set() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        assert!(config.is_complete());
    }

    // Mutex to serialize tests that mutate environment variables so they don't race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_inference_config_uses_anthropic_env_var_when_no_config() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_ant = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::set_var(
                "ANTHROPIC_API_KEY",
                fake_key(&["sk-", "ant-", "test-resolve"]),
            );
            std::env::remove_var("OPENAI_API_KEY");
        }

        let result = resolve_inference_config();

        unsafe {
            match &saved_ant {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
            match &saved_oai {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        // Config file (if complete) takes precedence over env var — either source is acceptable.
        let inf =
            result.expect("resolve_inference_config should succeed with ANTHROPIC_API_KEY set");
        assert!(inf.is_complete(), "resolved config must be complete");
    }

    #[test]
    fn resolve_inference_config_uses_openai_env_var_when_no_anthropic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_ant = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::set_var(
                "OPENAI_API_KEY",
                fake_key(&["sk-", "openai-", "test-resolve"]),
            );
        }

        let result = resolve_inference_config();

        unsafe {
            match &saved_ant {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
            match &saved_oai {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        let inf = result.expect("resolve_inference_config should succeed with OPENAI_API_KEY set");
        assert!(inf.is_complete(), "resolved config must be complete");
    }

    #[test]
    fn no_provider_configured_returns_error_in_non_tty() {
        // Wizard should fail with E-CFG-001 when no TTY is available.
        // Only meaningful if no config file has complete inference — skip otherwise.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_ant = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }

        let cfg = load_mur_config().ok();
        let has_complete_config = cfg
            .as_ref()
            .and_then(|c| c.inference.as_ref())
            .map(|i| i.is_complete())
            .unwrap_or(false);

        let result = if !has_complete_config {
            Some(run_wizard())
        } else {
            None
        };

        unsafe {
            match &saved_ant {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
            match &saved_oai {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
        }

        if let Some(result) = result {
            let err = result.expect_err("wizard should fail in non-TTY context");
            assert_eq!(err.code, "E-CFG-001", "should return E-CFG-001 in non-TTY");
        }
    }

    fn anthropic_inf() -> InferenceConfig {
        InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        }
    }

    fn openai_inf() -> InferenceConfig {
        InferenceConfig {
            provider: "openai".to_string(),
            model: "gpt-5.6-luna".to_string(),
            api_key: fake_key(&["sk-", "test"]),
            endpoint: String::new(),
        }
    }

    #[test]
    fn a_url_index_reaches_the_tool_as_its_index_input() {
        let url = "http://127.0.0.1:8080/x/artifacts-index.json";
        let index = GeneratorIndex::resolve(Some(url)).unwrap();
        for config in [anthropic_inf(), openai_inf()] {
            let prompt = build_task_prompt("summarise a document", index.tool_input(), &config);
            assert!(
                prompt.contains(&format!("\"index\": \"{url}\"")),
                "the example call passes the URL as `index`: {prompt}"
            );
            assert!(!prompt.contains("\"registry\""), "{prompt}");
        }
    }

    #[test]
    fn a_file_index_reaches_the_tool_as_its_work_folder_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-index.json");
        let text = "{\"schema_version\":2,\"scope\":\"ana\",\"artifacts\":[]}\n";
        std::fs::write(&path, text).unwrap();
        let index = GeneratorIndex::resolve(Some(path.to_str().unwrap())).unwrap();
        assert_eq!(
            index,
            GeneratorIndex::File {
                text: text.to_string()
            }
        );
        assert!(index.extra_network_allow().is_empty());
        for config in [anthropic_inf(), openai_inf()] {
            let prompt = build_task_prompt("summarise a document", index.tool_input(), &config);
            assert!(
                prompt.contains("\"index\": \"artifacts-index.json\""),
                "{prompt}"
            );
            assert!(!prompt.contains("\"registry\""), "{prompt}");
        }
    }

    #[test]
    fn no_index_names_the_latest_default_artifacts_release() {
        let index = GeneratorIndex::resolve(None).unwrap();
        assert_eq!(index, GeneratorIndex::Default);
        assert!(index.extra_network_allow().is_empty());
        for config in [anthropic_inf(), openai_inf()] {
            let prompt = build_task_prompt("summarise a document", index.tool_input(), &config);
            assert!(
                prompt.contains(&format!("\"index\": \"{DEFAULT_INDEX_URL}\"")),
                "{prompt}"
            );
            assert!(!prompt.contains("\"registry\""), "{prompt}");
        }
    }

    #[test]
    fn a_local_index_is_refused_naming_mur_search() {
        let error = GeneratorIndex::resolve(Some("local")).unwrap_err();
        assert_eq!(error.code, E_NEW_005);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("mur search <query> --index local")),
            "{error:?}"
        );
    }

    /// GitHub's raw-content host, which the generator manifest never names. Split so a search
    /// of the source for references to it finds none.
    const RETIRED_INDEX_HOST: &str = concat!("raw.", "githubusercontent.com");

    /// The parsed generator manifest's top-level `capabilities.network.allow`.
    fn network_allow(yaml: &str) -> Vec<String> {
        RuntimeManifest::from_yaml_str(yaml)
            .expect("the generator manifest parses")
            .capabilities
            .and_then(|capabilities| capabilities.network)
            .map(|network| network.allow)
            .unwrap_or_default()
    }

    #[test]
    fn build_meta_manifest_grants_the_registry_search_hosts() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &[]);
            assert_eq!(
                declared_version(&yaml, "murmur/tool-registry-search").as_deref(),
                Some("0.2.0")
            );
            assert_eq!(
                network_allow(&yaml),
                [
                    "https://github.com",
                    "https://release-assets.githubusercontent.com"
                ],
                "{} provider",
                config.provider
            );
            assert!(!yaml.contains(RETIRED_INDEX_HOST), "{yaml}");
        }
    }

    #[test]
    fn build_meta_manifest_appends_a_url_indexs_origin_once() {
        let index =
            GeneratorIndex::resolve(Some("http://127.0.0.1:8080/x/artifacts-index.json")).unwrap();
        let extra = index.extra_network_allow();
        assert_eq!(extra, ["http://127.0.0.1:8080"]);
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &extra);
            assert_eq!(
                network_allow(&yaml),
                [
                    "https://github.com",
                    "https://release-assets.githubusercontent.com",
                    "http://127.0.0.1:8080"
                ],
                "{} provider",
                config.provider
            );
            assert!(!yaml.contains(RETIRED_INDEX_HOST), "{yaml}");
            let mut manifest = RuntimeManifest::from_yaml_str(&yaml).unwrap();
            inject_driver_key(&mut manifest, "test-key");
            let driver = config_driver(&config);
            assert_eq!(
                manifest
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.name == driver)
                    .and_then(|artifact| artifact.gateway.as_ref())
                    .and_then(|gateway| gateway.api_key.clone()),
                Some(murmur_artifact::ApiKeyReference::Literal(
                    "test-key".to_string()
                ))
            );
        }
    }

    #[test]
    fn build_meta_manifest_adds_nothing_for_a_url_on_github() {
        let index = GeneratorIndex::resolve(Some(
            "https://github.com/ana/my-artifacts/releases/latest/download/artifacts-index.json",
        ))
        .unwrap();
        assert!(index.extra_network_allow().is_empty());
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &index.extra_network_allow());
            assert_eq!(
                network_allow(&yaml),
                [
                    "https://github.com",
                    "https://release-assets.githubusercontent.com"
                ]
            );
        }
    }

    fn config_driver(config: &InferenceConfig) -> &'static str {
        match config.provider.as_str() {
            "openai" => OPENAI.driver,
            _ => ANTHROPIC.driver,
        }
    }

    /// The `version:` of the artifact entry named `name` in a generator manifest.
    fn declared_version(yaml: &str, name: &str) -> Option<String> {
        RuntimeManifest::from_yaml_str(yaml)
            .expect("the generator manifest parses")
            .artifacts
            .iter()
            .find(|artifact| artifact.name == name)
            .map(|artifact| artifact.version.clone())
    }

    #[test]
    fn build_meta_manifest_pins_the_providers_driver_version() {
        for (config, provider) in [(anthropic_inf(), &ANTHROPIC), (openai_inf(), &OPENAI)] {
            let yaml = build_meta_manifest(&config, &[]);
            assert_eq!(
                declared_version(&yaml, provider.driver).as_deref(),
                Some(provider.driver_version),
                "the {} generator pins {} at Provider::driver_version",
                config.provider,
                provider.driver
            );
            assert!(
                yaml.contains(&format!("    artifact: {}\n", provider.driver)),
                "inference.driver.artifact names {}",
                provider.driver
            );
        }
    }

    #[test]
    fn build_meta_manifest_includes_tool_editor() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &[]);
            assert_eq!(
                declared_version(&yaml, "murmur/tool-editor").as_deref(),
                Some(GENERATOR_ARTIFACTS[1].version),
                "must include murmur/tool-editor ({} provider)",
                config.provider
            );
        }
    }

    #[test]
    fn build_meta_manifest_includes_tool_registry_search() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &[]);
            assert_eq!(
                declared_version(&yaml, "murmur/tool-registry-search").as_deref(),
                Some(GENERATOR_ARTIFACTS[0].version),
                "must include murmur/tool-registry-search ({} provider)",
                config.provider
            );
        }
    }

    #[test]
    fn build_meta_manifest_includes_skill_create_manifest() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config, &[]);
            let manifest = RuntimeManifest::from_yaml_str(&yaml).expect("parses");
            let skill = manifest
                .artifacts
                .iter()
                .find(|artifact| artifact.name == "murmur/skill-create-manifest")
                .unwrap_or_else(|| {
                    panic!(
                        "must include murmur/skill-create-manifest ({} provider)",
                        config.provider
                    )
                });
            assert_eq!(skill.version, GENERATOR_ARTIFACTS[2].version);
            assert_eq!(
                skill.runtime,
                ArtifactRuntime::Skill,
                "must have runtime: skill entry ({} provider)",
                config.provider
            );
        }
    }

    #[test]
    fn generator_pins_match_the_docs() {
        for artifact in &GENERATOR_ARTIFACTS {
            let docs_key = artifact.name.replace(['/', '-'], "_");
            let docs = crate::published_pins::docs_pin(&docs_key);
            assert_eq!(
                artifact.version, docs,
                "the generator pins {}@{}, and docs/mkdocs.yml extra.v.{docs_key} is {docs}",
                artifact.name, artifact.version
            );
        }
    }

    #[test]
    fn build_task_prompt_does_not_contain_inline_schema() {
        for config in [anthropic_inf(), openai_inf()] {
            let prompt = build_task_prompt("summarise a document", DEFAULT_INDEX_URL, &config);
            assert!(
                !prompt.contains("api_key:"),
                "prompt must not embed api_key inline schema field ({} provider)",
                config.provider
            );
            assert!(
                !prompt.contains("ANTHROPIC_API_KEY"),
                "prompt must not embed provider key vars ({} provider)",
                config.provider
            );
            assert!(
                !prompt.contains("OPENAI_API_KEY"),
                "prompt must not embed provider key vars ({} provider)",
                config.provider
            );
        }
    }

    #[test]
    fn build_task_prompt_instructs_read_file() {
        let prompt = build_task_prompt("summarise a document", DEFAULT_INDEX_URL, &anthropic_inf());
        assert!(
            prompt.contains("read_file"),
            "prompt must instruct agent to call read_file"
        );
        assert!(
            prompt.contains("tools/skill-create-manifest/skill.md"),
            "prompt must reference skill.md path"
        );
    }

    #[test]
    fn build_task_prompt_instructs_write_file() {
        let prompt = build_task_prompt("summarise a document", DEFAULT_INDEX_URL, &anthropic_inf());
        assert!(
            prompt.contains("write_file"),
            "prompt must instruct agent to call write_file"
        );
        assert!(
            prompt.contains("out/murmur.yaml"),
            "prompt must reference out/murmur.yaml"
        );
    }
}
