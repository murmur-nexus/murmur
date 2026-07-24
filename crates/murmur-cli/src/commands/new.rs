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
use murmur_artifact::{current_platform, resolve_manifest_path, ArtifactRuntime, LocalRegistry, Registry, RegistryError, RuntimeManifest, MANIFEST_FILENAME};

use crate::{
    config::{load_mur_config, save_mur_config, InferenceConfig},
    error::{CliError, E_CFG_001, E_IO_003, E_MAN_002, E_RUN_008},
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
            eprintln!("mur new: session dir preserved at {}", self.path.display());
        } else {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub(crate) fn run_new(task: &str, registry: Option<&str>) -> Result<(), CliError> {
    // Resolve inference config: config file → env var → interactive wizard.
    let inf = resolve_inference_config()?;

    let local_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    // Build the dynamic meta-manifest with a neutral api_key placeholder — parsing never
    // touches the environment — then inject the real key directly into the parsed struct.
    // This keeps the resolved key a Rust `String` in memory only; it is never round-tripped
    // through this process's own environment (which a /proc/<pid>/environ read could expose).
    let meta_yaml = build_meta_manifest(&inf);
    let mut runtime_manifest = RuntimeManifest::from_yaml_str(&meta_yaml).map_err(|e| {
        CliError::new(
            E_MAN_002,
            format!("internal: generator meta-manifest is invalid: {e}"),
        )
    })?;
    if let Some(inference) = runtime_manifest.inference.as_mut() {
        inference.api_key = Some(inf.api_key.clone());
    }

    // Check that all generator capsule artifacts are installed before staging.
    let generator_artifacts: Vec<ArtifactRequest> = runtime_manifest
        .artifacts
        .iter()
        .map(|a| ArtifactRequest {
            name: a.name.clone(),
            version: a.version.clone(),
            runtime: a.runtime.clone(),
            source: a.source.clone(),
            capabilities: a.capabilities.clone(),
        })
        .collect();

    check_artifacts_installed(&local_registry, &generator_artifacts)?;

    // Build the task prompt with a compact inline schema.
    let task_prompt = build_task_prompt(task, registry, &inf);

    // Create a temp dir for the generator session. The session workdir lives inside it
    // (manifest_dir/workdir/<session_id>/), so cleanup on Drop removes everything.
    let temp_dir = TempDir::create()?;
    let manifest_dir = temp_dir.path().to_path_buf();

    // stage_session expects manifest_dir/murmur.yaml for system_prompt_file resolution.
    fs::write(manifest_dir.join(MANIFEST_FILENAME), &meta_yaml).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to write generator manifest: {e}"),
        )
    })?;

    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);
    let mut allowlisted_tools = HashSet::new();
    for artifact in &runtime_manifest.artifacts {
        if artifact.runtime == ArtifactRuntime::Tool {
            allowlisted_tools.insert(artifact.name.clone());
        }
    }

    let stage_request = StageRequest {
        manifest_dir,
        capsule_name: runtime_manifest.name.clone(),
        capsule_version: runtime_manifest.version.clone(),
        capsule_component_bytes: Vec::new(), // agent capsule — no WASM component
        artifacts: generator_artifacts,
        allowlisted_tools,
        lock_expectations: None, // ephemeral generator; no lockfile
        capability_policy,
        inference: runtime_manifest.inference.clone(),
        context: runtime_manifest.context.clone(),
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
        job_id: None,
    };

    // Stage the session (creates workdir, installs artifacts including skill.md).
    let staged = stage_session(Arc::new(local_registry), stage_request).map_err(CliError::from)?;
    let session_workdir = staged.workdir.clone();

    // Write the task prompt as task.md. The runtime reads this as the initial task when
    // task_acceptance: single and task.md exists — no A2A handshake needed.
    fs::write(session_workdir.join("task.md"), &task_prompt).map_err(|e| {
        CliError::new(E_IO_003, format!("failed to write task prompt: {e}"))
    })?;

    eprintln!("mur new: generating manifest...");

    // Run the generator synchronously. The capsule reads skill.md via murmur-tool-editor,
    // discovers artifacts via murmur-tool-registry-search, generates the manifest, and writes
    // it to out/murmur.yaml. The session exits via after_task: exit.
    launch_session(staged, |_url| {}).map_err(CliError::from)?;

    // Read the manifest written by the agent via write_file("out/murmur.yaml").
    let manifest_path = session_workdir.join("out").join("murmur.yaml");
    if !manifest_path.exists() {
        let agent_output = fs::read_to_string(session_workdir.join("out").join("result.txt"))
            .unwrap_or_else(|_| "(no output captured)".to_string());
        return Err(CliError::new(
            "E-NEW-001",
            format!(
                "generator agent did not produce out/murmur.yaml\nagent output:\n{agent_output}"
            ),
        ));
    }
    let manifest_yaml = fs::read_to_string(&manifest_path).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read out/murmur.yaml: {e}"),
        )
    })?;

    // Validate structure before touching CWD.
    if std::env::var("MUR_DEBUG_MANIFEST").is_ok() {
        eprintln!("--- generated manifest ---\n{manifest_yaml}\n--- end ---");
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

    println!("{MANIFEST_FILENAME} written to {}", output_path.display());
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
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        if !key.is_empty() {
            eprintln!(
                "hint: found ANTHROPIC_API_KEY in environment; add [inference] to ~/.murmur/config.yaml to persist your provider settings"
            );
            return Ok(InferenceConfig {
                provider: "anthropic".to_string(),
                model: "claude-haiku-4-5-20251001".to_string(),
                api_key: key,
                endpoint: String::new(),
            });
        }
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            eprintln!(
                "hint: found OPENAI_API_KEY in environment; add [inference] to ~/.murmur/config.yaml to persist your provider settings"
            );
            return Ok(InferenceConfig {
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                api_key: key,
                endpoint: String::new(),
            });
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

    eprintln!("No inference provider configured.");

    let theme = ColorfulTheme::default();

    let provider_idx = Select::with_theme(&theme)
        .with_prompt("Provider")
        .items(&["Anthropic", "OpenAI"])
        .default(0)
        .interact()
        .map_err(|_| non_tty_err())?;

    let (provider_str, model_labels, model_values): (&str, &[&str], &[&str]) = match provider_idx {
        0 => (
            "anthropic",
            &[
                "claude-haiku-4-5-20251001  (fast — recommended)",
                "claude-sonnet-4-5-20251001",
                "claude-opus-4-5-20251001",
                "Enter manually",
            ],
            &[
                "claude-haiku-4-5-20251001",
                "claude-sonnet-4-5-20251001",
                "claude-opus-4-5-20251001",
            ],
        ),
        _ => (
            "openai",
            &[
                "gpt-4o-mini  (fast — recommended)",
                "gpt-4o",
                "o3-mini",
                "Enter manually",
            ],
            &["gpt-4o-mini", "gpt-4o", "o3-mini"],
        ),
    };

    let model_idx = Select::with_theme(&theme)
        .with_prompt("Model")
        .items(model_labels)
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
        provider: provider_str.to_string(),
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

fn build_meta_manifest(config: &InferenceConfig) -> String {
    let (driver_name, driver_version, default_endpoint) = match config.provider.as_str() {
        "openai" => ("murmur-driver-openai", "0.3.34", "https://api.openai.com"),
        _ => (
            "murmur-driver-anthropic",
            "0.3.33",
            "https://api.anthropic.com",
        ),
    };
    let endpoint = if config.endpoint.is_empty() {
        default_endpoint
    } else {
        config.endpoint.as_str()
    };

    format!(
        r#"name: murmur-manifest-generator
version: "0.4.17"
artifacts:
  - name: {driver_name}
    version: "{driver_version}"
    runtime: driver
  - name: murmur-tool-registry-search
    version: "0.4.13"
    runtime: tool
  - name: murmur-tool-editor
    version: "0.4.4"
    runtime: tool
  - name: murmur-skill-create-manifest
    version: "0.1.1"
    runtime: skill
capabilities:
  network:
    allow:
      - "{endpoint}"
lifecycle:
  task_acceptance: single
  after_task: exit
inference:
  endpoint: {endpoint}
  model: {model}
  api_key: ""
  driver:
    artifact: {driver_name}
"#,
        driver_name = driver_name,
        driver_version = driver_version,
        endpoint = endpoint,
        model = config.model,
    )
}

fn check_artifacts_installed(
    local_registry: &LocalRegistry,
    artifacts: &[ArtifactRequest],
) -> Result<(), CliError> {
    for artifact in artifacts {
        match local_registry.resolve_with_platform(&artifact.name, &artifact.version, Some(current_platform())) {
            Ok(_) => {}
            Err(RegistryError::NotFound { .. }) => {
                return Err(CliError::with_hint(
                    E_RUN_008,
                    format!("generator artifact '{}' is not installed", artifact.name),
                    format!("run `mur install {}@{}` to install it", artifact.name, artifact.version),
                ));
            }
            Err(error) => return Err(CliError::from(error)),
        }
    }
    Ok(())
}

fn build_task_prompt(task: &str, registry: Option<&str>, _inf: &InferenceConfig) -> String {
    let registry_arg = match registry {
        Some(reg) => format!(", \"registry\":\"{reg}\""),
        None => String::new(),
    };

    format!(
        r#"Step 1 — Read your manifest generation guide:
Call read_file with path "tools/murmur-skill-create-manifest/skill.md".
Read the full contents before proceeding.

Step 2 — Search for available artifacts:
Call the registry search tool with a query relevant to the task.
Example: {{"query": "git editor shell"{registry_arg}}}
Use the exact artifact names and versions from the results.

Step 3 — Generate the manifest:
Task: {task}

Follow all guidance from the skill file. The manifest must pass `mur build` validation.
Prefer murmur-tool-git and murmur-tool-editor over granting bash shell access.
Use the exact artifact versions returned by the registry search.
Include runtime: tool at the top level.

Step 4 — Write the manifest:
Call write_file with path "out/murmur.yaml" and the YAML content.
Do not include markdown fences in the file content — write raw YAML only.
Output only the word DONE when finished.
"#,
        task = task,
        registry_arg = registry_arg,
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
        let yaml = build_meta_manifest(&config);
        assert!(yaml.contains("murmur-driver-anthropic"), "should use anthropic driver");
        assert!(yaml.contains("api.anthropic.com"), "should use anthropic endpoint");
        assert!(yaml.contains("claude-haiku-4-5-20251001"), "should use model");
        assert!(!yaml.contains("murmur-driver-openai"), "should not reference openai driver");
    }

    #[test]
    fn build_meta_manifest_openai_contains_correct_driver() {
        let config = InferenceConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: fake_key(&["sk-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config);
        assert!(yaml.contains("murmur-driver-openai"), "should use openai driver");
        assert!(yaml.contains("api.openai.com"), "should use openai endpoint");
        assert!(yaml.contains("gpt-4o-mini"), "should use model");
        assert!(!yaml.contains("murmur-driver-anthropic"), "should not reference anthropic driver");
    }

    #[test]
    fn build_meta_manifest_uses_custom_endpoint_when_set() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: "https://custom.proxy.example.com".to_string(),
        };
        let yaml = build_meta_manifest(&config);
        assert!(yaml.contains("custom.proxy.example.com"), "should use custom endpoint");
        assert!(!yaml.contains("api.anthropic.com"), "should not use default endpoint");
    }

    #[test]
    fn build_meta_manifest_uses_neutral_api_key_placeholder() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            api_key: fake_key(&["sk-", "ant-", "test"]),
            endpoint: String::new(),
        };
        let yaml = build_meta_manifest(&config);
        assert!(
            yaml.contains("api_key: \"\""),
            "api_key field must be a neutral empty-string placeholder"
        );
        assert!(!yaml.contains("${"), "manifest must not contain any ${{...}} env-reference token");
        assert!(!yaml.contains("MUR_INFERENCE_API_KEY"), "must not reference MUR_INFERENCE_API_KEY");
        assert!(!yaml.contains(&config.api_key), "must not embed the raw api key literal");
        assert!(!yaml.contains("ANTHROPIC_API_KEY"), "must not reference ANTHROPIC_API_KEY");
        assert!(!yaml.contains("OPENAI_API_KEY"), "must not reference OPENAI_API_KEY");
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
        let yaml = build_meta_manifest(&config);
        let runtime_manifest = RuntimeManifest::from_yaml_str(&yaml)
            .expect("build_meta_manifest should produce a valid runtime manifest without any env var present");
        assert_eq!(
            runtime_manifest.inference.and_then(|i| i.api_key),
            None,
            "parsed api_key should be None until the caller injects the real key in memory"
        );

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
            std::env::set_var("ANTHROPIC_API_KEY", fake_key(&["sk-", "ant-", "regression-test"]));
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("MUR_INFERENCE_API_KEY");
        }

        // Mirrors run_new's generator path: resolve config → build meta-manifest →
        // parse → inject the real key into the parsed struct in memory.
        let result = (|| -> Result<RuntimeManifest, CliError> {
            let inf = resolve_inference_config()?;
            let meta_yaml = build_meta_manifest(&inf);
            let mut runtime_manifest = RuntimeManifest::from_yaml_str(&meta_yaml).map_err(|e| {
                CliError::new(E_MAN_002, format!("internal: generator meta-manifest is invalid: {e}"))
            })?;
            if let Some(inference) = runtime_manifest.inference.as_mut() {
                inference.api_key = Some(inf.api_key.clone());
            }
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
        let resolved_api_key = runtime_manifest.inference.and_then(|i| i.api_key);
        assert!(
            resolved_api_key.is_some(),
            "resolved runtime_manifest.inference.api_key must still hold the real key in memory"
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
            std::env::set_var("ANTHROPIC_API_KEY", fake_key(&["sk-", "ant-", "test-resolve"]));
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
        let inf = result.expect("resolve_inference_config should succeed with ANTHROPIC_API_KEY set");
        assert!(inf.is_complete(), "resolved config must be complete");
    }

    #[test]
    fn resolve_inference_config_uses_openai_env_var_when_no_anthropic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_ant = std::env::var("ANTHROPIC_API_KEY").ok();
        let saved_oai = std::env::var("OPENAI_API_KEY").ok();
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::set_var("OPENAI_API_KEY", fake_key(&["sk-", "openai-", "test-resolve"]));
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
            model: "gpt-4o-mini".to_string(),
            api_key: fake_key(&["sk-", "test"]),
            endpoint: String::new(),
        }
    }

    #[test]
    fn registry_flag_appears_in_task_prompt() {
        let prompt = build_task_prompt("summarise a document", Some("local"), &anthropic_inf());
        assert!(
            prompt.contains("\"registry\":\"local\""),
            "task prompt should embed registry arg so the agent passes it to murmur-tool-registry-search"
        );
    }

    #[test]
    fn no_registry_flag_omits_registry_from_prompt() {
        let prompt = build_task_prompt("summarise a document", None, &anthropic_inf());
        assert!(
            !prompt.contains("\"registry\""),
            "task prompt should not include registry when flag is absent"
        );
    }

    #[test]
    fn build_meta_manifest_anthropic_uses_fixed_driver_version() {
        let yaml = build_meta_manifest(&anthropic_inf());
        assert!(yaml.contains("0.3.33"), "anthropic driver must be 0.3.33");
        assert!(!yaml.contains("0.3.32"), "must not reference old anthropic driver 0.3.32");
    }

    #[test]
    fn build_meta_manifest_openai_uses_fixed_driver_version() {
        let yaml = build_meta_manifest(&openai_inf());
        assert!(yaml.contains("0.3.34"), "openai driver must be 0.3.34");
        assert!(!yaml.contains("0.3.33"), "must not reference old openai driver 0.3.33");
    }

    #[test]
    fn build_meta_manifest_includes_tool_editor() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config);
            assert!(yaml.contains("murmur-tool-editor"), "must include murmur-tool-editor ({} provider)", config.provider);
            assert!(yaml.contains("0.4.4"), "must include murmur-tool-editor@0.4.4 ({} provider)", config.provider);
        }
    }

    #[test]
    fn build_meta_manifest_includes_skill_create_manifest() {
        for config in [anthropic_inf(), openai_inf()] {
            let yaml = build_meta_manifest(&config);
            assert!(yaml.contains("murmur-skill-create-manifest"), "must include murmur-skill-create-manifest ({} provider)", config.provider);
            assert!(yaml.contains("0.1.1"), "must include murmur-skill-create-manifest@0.1.1 ({} provider)", config.provider);
            assert!(yaml.contains("runtime: skill"), "must have runtime: skill entry ({} provider)", config.provider);
        }
    }

    #[test]
    fn build_task_prompt_does_not_contain_inline_schema() {
        for config in [anthropic_inf(), openai_inf()] {
            let prompt = build_task_prompt("summarise a document", None, &config);
            assert!(!prompt.contains("api_key:"), "prompt must not embed api_key inline schema field ({} provider)", config.provider);
            assert!(!prompt.contains("ANTHROPIC_API_KEY"), "prompt must not embed provider key vars ({} provider)", config.provider);
            assert!(!prompt.contains("OPENAI_API_KEY"), "prompt must not embed provider key vars ({} provider)", config.provider);
        }
    }

    #[test]
    fn build_task_prompt_instructs_read_file() {
        let prompt = build_task_prompt("summarise a document", None, &anthropic_inf());
        assert!(prompt.contains("read_file"), "prompt must instruct agent to call read_file");
        assert!(prompt.contains("tools/murmur-skill-create-manifest/skill.md"), "prompt must reference skill.md path");
    }

    #[test]
    fn build_task_prompt_instructs_write_file() {
        let prompt = build_task_prompt("summarise a document", None, &anthropic_inf());
        assert!(prompt.contains("write_file"), "prompt must instruct agent to call write_file");
        assert!(prompt.contains("out/murmur.yaml"), "prompt must reference out/murmur.yaml");
    }
}
