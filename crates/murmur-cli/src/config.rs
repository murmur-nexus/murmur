use std::{
    env, fs,
    path::{Path, PathBuf},
};

use murmur_artifact::{ContainmentClass, LocalRegistry, Registry};
use serde::{Deserialize, Serialize};

use crate::{
    error::{CliError, E_IO_001, E_IO_003},
    registry_client::RemoteRegistry,
};

pub(crate) const DEFAULT_REMOTE_REGISTRY: &str = "http://localhost:7800";

/// Runtime feature flag state. Persisted under `beta:` in ~/.murmur/config.yaml.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct BetaConfig {
    /// Names of beta features the user has explicitly enabled.
    #[serde(default)]
    pub enabled: Vec<String>,
}

impl BetaConfig {
    pub fn is_enabled(&self, feature: &str) -> bool {
        self.enabled.iter().any(|f| f == feature)
    }

    /// Returns true if newly added (false if already present).
    pub fn enable(&mut self, feature: &str) -> bool {
        if self.is_enabled(feature) {
            false
        } else {
            self.enabled.push(feature.to_string());
            true
        }
    }

    /// Returns true if it was present (false if already absent).
    pub fn disable(&mut self, feature: &str) -> bool {
        let before = self.enabled.len();
        self.enabled.retain(|f| f != feature);
        self.enabled.len() < before
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MurConfig {
    #[serde(default)]
    pub registry: RegistryConfig,
    #[serde(default)]
    pub inference: Option<InferenceConfig>,
    #[serde(default)]
    pub beta: BetaConfig,
    /// Workspace-wide minimum containment class. `None` means this workspace states no
    /// requirement — it does not mean `advisory`, so a manifest or `--containment` that asks
    /// for more is not contradicted. Merged as a max, never a project-wins override (see
    /// [`merge_containment`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentClass>,
}

/// The default artifact source is a Rust literal compiled into the `murmur-cli`
/// binary — it is never fetched over the network at install time or any other
/// point. `load_mur_config`/
/// `load_mur_config_if_exists` only ever read this value's override from a local
/// file (`~/.murmur/config.yaml`) via `fs::read_to_string`; if that file is
/// absent, this literal is used as-is. Do not change this impl to read a value
/// obtained from an HTTP call, environment-provided URL, or any other
/// network-reachable source.
impl Default for MurConfig {
    fn default() -> Self {
        Self {
            registry: RegistryConfig {
                default: Some("official".to_string()),
                sources: vec![SourceConfig {
                    name: "official".to_string(),
                    r#type: SourceType::GitHub,
                    repo: Some("murmur-nexus/default-artifacts".to_string()),
                    url: None,
                    token: None,
                }],
                index_url: None,
            },
            inference: None,
            beta: BetaConfig::default(),
            containment: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct InferenceConfig {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub endpoint: String,
}

#[cfg(feature = "beta-mur-new")]
impl InferenceConfig {
    pub fn is_complete(&self) -> bool {
        (self.provider == "anthropic" || self.provider == "openai")
            && !self.model.is_empty()
            && !self.api_key.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RegistryConfig {
    pub default: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    /// Override URL for the public artifact index fetched by `mur search`.
    /// Key: registry.index_url in ~/.murmur/config.yaml
    #[serde(default)]
    pub index_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SourceConfig {
    pub name: String,
    pub r#type: SourceType,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

impl SourceConfig {
    pub fn resolved_token(&self) -> Option<String> {
        let raw = self.token.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }

        if let Some(var) = parse_env_reference(raw) {
            return env::var(var).ok();
        }

        env::var(raw).ok().or_else(|| Some(raw.to_string()))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    GitHub,
}

#[derive(Debug, Deserialize, Default)]
struct WorkspaceConfig {
    #[serde(default)]
    registry: WorkspaceRegistryConfig,
}

#[derive(Debug, Deserialize, Default)]
struct WorkspaceRegistryConfig {
    default: Option<String>,
    remote_url: Option<String>,
}

pub(crate) enum RegistryMode {
    Local,
    Remote { url: String },
}

pub(crate) fn resolve_registry(
    registry_override: Option<&str>,
) -> Result<Box<dyn Registry>, CliError> {
    let config = load_workspace_config()?;
    let mode = resolve_registry_mode(registry_override, &config)?;

    match mode {
        RegistryMode::Local => Ok(Box::new(
            LocalRegistry::from_default_home().map_err(CliError::from)?,
        )),
        RegistryMode::Remote { url } => {
            let api_key = std::env::var("NEXUS_API_KEY").map_err(|_| {
                CliError::new(
                    E_IO_003,
                    "NEXUS_API_KEY is required for remote registry mode. Set it or use local mode.",
                )
            })?;
            Ok(Box::new(RemoteRegistry::new(url, api_key)))
        }
    }
}

fn resolve_registry_mode(
    registry_override: Option<&str>,
    config: &WorkspaceConfig,
) -> Result<RegistryMode, CliError> {
    if let Some(url) = registry_override {
        if url.trim().eq_ignore_ascii_case("local") {
            return Ok(RegistryMode::Local);
        }
        return Ok(RegistryMode::Remote {
            url: url.trim().trim_end_matches('/').to_string(),
        });
    }

    if let Some(remote_url) = config
        .registry
        .remote_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(RegistryMode::Remote {
            url: remote_url.trim_end_matches('/').to_string(),
        });
    }

    match config.registry.default.as_deref() {
        None | Some("local") => Ok(RegistryMode::Local),
        Some("remote") => Ok(RegistryMode::Remote {
            url: DEFAULT_REMOTE_REGISTRY.to_string(),
        }),
        Some(other) => Err(CliError::new(
            E_IO_003,
            format!(
                "invalid registry.default '{}' (expected local|remote)",
                other
            ),
        )),
    }
}

fn load_workspace_config() -> Result<WorkspaceConfig, CliError> {
    let config_path = workspace_config_path()?;
    if !config_path.exists() {
        return Ok(WorkspaceConfig::default());
    }

    let raw = fs::read_to_string(&config_path).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {source}", config_path.display()),
        )
    })?;

    serde_yaml::from_str::<WorkspaceConfig>(&raw).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to parse {}: {source}", config_path.display()),
        )
    })
}

pub fn load_mur_config() -> Result<MurConfig, CliError> {
    load_mur_config_if_exists().map(|opt| opt.unwrap_or_default())
}

pub fn load_mur_config_if_exists() -> Result<Option<MurConfig>, CliError> {
    read_mur_config_file(&mur_config_path()?)
}

/// Reads `<cwd>/.murmur/config.yaml`. Lookup is cwd-only — unlike
/// `install.rs::find_project_root()`, this does not walk up parent directories.
pub fn load_project_mur_config_if_exists() -> Result<Option<MurConfig>, CliError> {
    read_mur_config_file(&project_mur_config_path()?)
}

pub fn save_mur_config(config: &MurConfig) -> Result<(), CliError> {
    write_mur_config_file(&mur_config_path()?, config)
}

pub fn save_project_mur_config(config: &MurConfig) -> Result<(), CliError> {
    write_mur_config_file(&project_mur_config_path()?, config)
}

/// Loads the effective `MurConfig`: the global (`~/.murmur/config.yaml`) merged with
/// the project-level (`<cwd>/.murmur/config.yaml`) file per-key (see `merge_mur_configs`).
/// When neither file exists, this is field-for-field identical to `load_mur_config()`.
pub fn load_effective_mur_config() -> Result<MurConfig, CliError> {
    load_effective_mur_config_if_any_exists().map(|opt| opt.unwrap_or_default())
}

/// Same as `load_effective_mur_config`, but returns `None` when neither the global nor
/// the project-level file exists (mirrors `load_mur_config_if_exists`'s None/Some contract
/// for call sites that skip building a source-chain fallback when there is no config at all).
pub fn load_effective_mur_config_if_any_exists() -> Result<Option<MurConfig>, CliError> {
    let global_opt = load_mur_config_if_exists()?;
    let project_opt = load_project_mur_config_if_exists()?;

    if global_opt.is_none() && project_opt.is_none() {
        return Ok(None);
    }

    if let Some(project) = &project_opt {
        warn_if_project_api_key_literal(project)?;
    }

    let global = global_opt.unwrap_or_default();
    Ok(Some(merge_mur_configs(global, project_opt)))
}

/// Per-key merge of a global and an optional project-level `MurConfig`. `project` values win
/// for non-empty scalars; `registry.sources` and `beta.enabled` merge as unions; `api_key` is
/// unconditionally sourced from `global`, never `project`, regardless of its shape. Returns
/// `global` unchanged when `project` is `None`.
pub fn merge_mur_configs(global: MurConfig, project: Option<MurConfig>) -> MurConfig {
    let Some(project) = project else {
        return global;
    };

    MurConfig {
        registry: RegistryConfig {
            default: pick_non_empty(project.registry.default, global.registry.default),
            sources: merge_sources(global.registry.sources, project.registry.sources),
            index_url: pick_non_empty(project.registry.index_url, global.registry.index_url),
        },
        inference: merge_inference(global.inference, project.inference),
        beta: BetaConfig {
            enabled: merge_beta_enabled(global.beta.enabled, project.beta.enabled),
        },
        containment: merge_containment(global.containment, project.containment),
    }
}

/// Merges the two `containment` declarations as a **max**, deliberately breaking this file's
/// project-wins rule: a containment class is a floor, so a project file must be able to raise
/// what the global file asked for but never to lower it. `None` on a side means that side asked
/// for nothing and does not participate; `None` on both stays `None` (no declaration at all,
/// which is distinct from an explicit `advisory`).
pub fn merge_containment(
    global: Option<ContainmentClass>,
    project: Option<ContainmentClass>,
) -> Option<ContainmentClass> {
    match (global, project) {
        (Some(global), Some(project)) => Some(global.max(project)),
        (value, None) | (None, value) => value,
    }
}

/// `project`, if `Some` and non-empty, otherwise `global`.
fn pick_non_empty(project: Option<String>, global: Option<String>) -> Option<String> {
    match project {
        Some(value) if !value.is_empty() => Some(value),
        _ => global,
    }
}

/// Union of `registry.sources`, de-duplicated by `name`: a project entry replaces a global
/// entry with the same name, a project entry with a new name is appended, and every global
/// entry the project doesn't mention is kept (global ordering first, new names appended).
fn merge_sources(global: Vec<SourceConfig>, project: Vec<SourceConfig>) -> Vec<SourceConfig> {
    let mut merged = global;
    for source in project {
        match merged
            .iter_mut()
            .find(|existing| existing.name == source.name)
        {
            Some(existing) => *existing = source,
            None => merged.push(source),
        }
    }
    merged
}

/// Union of `beta.enabled`, de-duplicated by string value: global flags first, then any
/// project-only flags appended in the order they appear in the project file.
fn merge_beta_enabled(global: Vec<String>, project: Vec<String>) -> Vec<String> {
    let mut merged = global;
    for flag in project {
        if !merged.contains(&flag) {
            merged.push(flag);
        }
    }
    merged
}

/// `provider`/`model`/`endpoint` follow the same non-empty-wins rule as the registry scalars.
/// `api_key` is unconditionally sourced from `global` (or `""` if `global` has no inference
/// block) — never from `project`, even if `project`'s value is an env-var reference.
fn merge_inference(
    global: Option<InferenceConfig>,
    project: Option<InferenceConfig>,
) -> Option<InferenceConfig> {
    match (global, project) {
        (None, None) => None,
        (Some(global), None) => Some(global),
        (global, Some(project)) => {
            let global = global.unwrap_or_default();
            Some(InferenceConfig {
                provider: if !project.provider.is_empty() {
                    project.provider
                } else {
                    global.provider
                },
                model: if !project.model.is_empty() {
                    project.model
                } else {
                    global.model
                },
                api_key: global.api_key,
                endpoint: if !project.endpoint.is_empty() {
                    project.endpoint
                } else {
                    global.endpoint
                },
            })
        }
    }
}

/// Prints a warning to stderr naming `project_mur_config_path()` and the `inference.api_key`
/// field when the project-level file sets a *literal* (non-`${VAR}`) api_key. Silent for a
/// `${VAR}`-style reference — either way the project value is never honored (see
/// `merge_inference`).
fn warn_if_project_api_key_literal(project: &MurConfig) -> Result<(), CliError> {
    let Some(inference) = &project.inference else {
        return Ok(());
    };

    if !is_literal_inference_api_key(&inference.api_key) {
        return Ok(());
    }

    let path = project_mur_config_path()?;
    eprintln!(
        "warning: {} sets inference.api_key to a literal value, but inference.api_key is \
         always read from the global config (~/.murmur/config.yaml); this project-level value \
         will be ignored",
        path.display()
    );
    Ok(())
}

/// True when `value` is non-empty and is not a `${VAR}` env-var reference — i.e. it would be
/// silently ignored (and, on read, warned about) as a project-level `inference.api_key`.
pub(crate) fn is_literal_inference_api_key(value: &str) -> bool {
    !value.is_empty() && parse_env_reference(value).is_none()
}

fn read_mur_config_file(config_path: &Path) -> Result<Option<MurConfig>, CliError> {
    if !config_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(config_path).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {source}", config_path.display()),
        )
    })?;

    serde_yaml::from_str::<MurConfig>(&raw)
        .map(Some)
        .map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to parse {}: {source}", config_path.display()),
            )
        })
}

fn write_mur_config_file(config_path: &Path, config: &MurConfig) -> Result<(), CliError> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to create {}: {source}", parent.display()),
            )
        })?;
    }

    let serialized = serde_yaml::to_string(config).map_err(|source| {
        CliError::new(E_IO_003, format!("failed to serialize config: {source}"))
    })?;

    let tmp_path = config_path.with_file_name(".config.yaml.tmp");
    fs::write(&tmp_path, &serialized).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to write {}: {source}", tmp_path.display()),
        )
    })?;
    fs::rename(&tmp_path, config_path).map_err(|source| {
        let _ = fs::remove_file(&tmp_path);
        CliError::new(
            E_IO_003,
            format!("failed to rename to {}: {source}", config_path.display()),
        )
    })?;

    Ok(())
}

fn workspace_config_path() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir().map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current working directory: {source}"),
        )
    })?;
    Ok(cwd.join("murmur.yaml"))
}

/// Build a `RemoteRegistry` pointing at the URL from `murmur.yaml` (or the
/// built-in default) using `NEXUS_API_KEY` from the environment.
// Remote-registry mode has no command wired to it yet; this is the single place that resolves
// the URL and `NEXUS_API_KEY` for when one is, and deleting it would scatter that resolution.
#[allow(dead_code)]
pub(crate) fn build_remote_registry() -> Result<RemoteRegistry, CliError> {
    let config = load_workspace_config()?;
    let url = config
        .registry
        .remote_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_REMOTE_REGISTRY.to_string());

    let api_key = std::env::var("NEXUS_API_KEY").map_err(|_| {
        CliError::new(
            E_IO_003,
            "NEXUS_API_KEY is required for remote registry mode. Set it or use local mode.",
        )
    })?;

    Ok(RemoteRegistry::new(url, api_key))
}

pub fn mur_config_path() -> Result<PathBuf, CliError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| CliError::new(E_IO_001, "could not determine home directory"))?;

    let mut base = PathBuf::from(home);
    if !base.is_absolute() {
        base = env::current_dir()
            .map_err(|source| {
                CliError::new(
                    E_IO_003,
                    format!("failed to determine current working directory: {source}"),
                )
            })?
            .join(base);
    }

    Ok(base.join(".murmur").join("config.yaml"))
}

/// `<cwd>/.murmur/config.yaml`. Lookup is cwd-only by design — see the module doc on
/// `load_project_mur_config_if_exists`; a future slice may add walk-up discovery.
pub fn project_mur_config_path() -> Result<PathBuf, CliError> {
    let cwd = env::current_dir().map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current working directory: {source}"),
        )
    })?;
    Ok(cwd.join(".murmur").join("config.yaml"))
}

fn parse_env_reference(value: &str) -> Option<&str> {
    if !value.starts_with("${") || !value.ends_with('}') {
        return None;
    }

    let variable = &value[2..value.len() - 1];
    is_valid_env_variable(variable).then_some(variable)
}

fn is_valid_env_variable(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !(first == '_' || first.is_ascii_uppercase()) {
        return false;
    }

    chars.all(|ch| ch == '_' || ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

// Mutex to serialize tests (in this module and elsewhere in the crate, e.g.
// commands/config_cmd.rs) that mutate the process-wide HOME env var and/or current
// working directory, so concurrently-run tests don't race on shared process state.
#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Build key-shaped test values at runtime so the source never contains a
    /// credential-shaped literal that secret scanners could flag.
    fn fake_key(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn default_registry_source_is_compiled_in_literal_not_fetched() {
        let config = MurConfig::default();

        assert_eq!(config.registry.default.as_deref(), Some("official"));
        assert_eq!(config.registry.sources.len(), 1);

        let source = &config.registry.sources[0];
        assert_eq!(source.name, "official");
        assert_eq!(source.r#type, SourceType::GitHub);
        assert_eq!(
            source.repo.as_deref(),
            Some("murmur-nexus/default-artifacts")
        );
        assert_eq!(source.url, None);
        assert_eq!(source.token, None);
    }

    #[test]
    fn local_config_file_overrides_compiled_in_default() {
        let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_home = env::var_os("HOME");

        let tmp_home = tempfile::tempdir().expect("failed to create temp HOME");
        let murmur_dir = tmp_home.path().join(".murmur");
        fs::create_dir_all(&murmur_dir).expect("failed to create .murmur dir");
        fs::write(
            murmur_dir.join("config.yaml"),
            r#"
registry:
  default: official
  sources:
    - name: official
      type: github
      repo: some-user/custom-artifacts
"#,
        )
        .expect("failed to write config.yaml");

        // SAFETY: `HOME_ENV_LOCK` serializes every test in this module that mutates
        // process-wide env vars, so no other thread observes a torn value.
        unsafe {
            env::set_var("HOME", tmp_home.path());
        }

        let result = load_mur_config();

        // SAFETY: still holding `_guard`; restores the pre-test HOME value.
        unsafe {
            match &saved_home {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
        }

        let config = result.expect("load_mur_config should succeed");
        assert_eq!(
            config.registry.sources[0].repo.as_deref(),
            Some("some-user/custom-artifacts")
        );
    }

    fn source(name: &str, repo: &str) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            r#type: SourceType::GitHub,
            repo: Some(repo.to_string()),
            url: None,
            token: None,
        }
    }

    #[test]
    fn merge_with_no_project_config_returns_global_unchanged() {
        let global = MurConfig::default();
        let merged = merge_mur_configs(global.clone(), None);
        assert_eq!(merged.registry.default, global.registry.default);
        assert_eq!(merged.registry.sources.len(), global.registry.sources.len());
        assert_eq!(merged.beta.enabled, global.beta.enabled);
    }

    #[test]
    fn merge_scalar_override_registry_default() {
        let global = MurConfig::default();
        let mut project = MurConfig::default();
        project.registry.default = Some("local".to_string());
        project.registry.sources = vec![];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(merged.registry.default.as_deref(), Some("local"));
        // untouched global source survives
        assert_eq!(merged.registry.sources.len(), 1);
        assert_eq!(merged.registry.sources[0].name, "official");
    }

    #[test]
    fn merge_sources_union_appends_new_name() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("official", "a/b")];
        let mut project = MurConfig::default();
        project.registry.sources = vec![source("mine", "me/repo")];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(merged.registry.sources.len(), 2);
        assert!(merged.registry.sources.iter().any(|s| s.name == "official"));
        assert!(merged.registry.sources.iter().any(|s| s.name == "mine"));
    }

    #[test]
    fn merge_sources_override_by_name_replaces_not_duplicates() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("official", "a/b")];
        let mut project = MurConfig::default();
        project.registry.sources = vec![source("official", "fork/b")];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(merged.registry.sources.len(), 1);
        assert_eq!(merged.registry.sources[0].repo.as_deref(), Some("fork/b"));
    }

    #[test]
    fn merge_beta_enabled_is_a_union_not_override() {
        let mut global = MurConfig::default();
        global.beta.enabled = vec!["deploy".to_string()];
        let mut project = MurConfig::default();
        project.beta.enabled = vec!["new".to_string()];

        let merged = merge_mur_configs(global, Some(project));

        assert!(merged.beta.is_enabled("deploy"));
        assert!(merged.beta.is_enabled("new"));
        assert_eq!(merged.beta.enabled.len(), 2);
    }

    #[test]
    fn merge_inference_api_key_always_from_global_literal_case() {
        let global = MurConfig {
            inference: Some(InferenceConfig {
                provider: "anthropic".to_string(),
                model: "claude".to_string(),
                api_key: "global-key".to_string(),
                endpoint: String::new(),
            }),
            ..MurConfig::default()
        };
        let project = MurConfig {
            inference: Some(InferenceConfig {
                api_key: fake_key(&["sk-", "live-", "abc123"]),
                ..InferenceConfig::default()
            }),
            ..MurConfig::default()
        };

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(
            merged.inference.as_ref().map(|i| i.api_key.as_str()),
            Some("global-key")
        );
    }

    #[test]
    fn merge_inference_api_key_always_from_global_env_ref_case() {
        let global = MurConfig {
            inference: Some(InferenceConfig {
                api_key: "global-key".to_string(),
                ..InferenceConfig::default()
            }),
            ..MurConfig::default()
        };
        let project = MurConfig {
            inference: Some(InferenceConfig {
                api_key: "${SOME_VAR}".to_string(),
                ..InferenceConfig::default()
            }),
            ..MurConfig::default()
        };

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(
            merged.inference.as_ref().map(|i| i.api_key.as_str()),
            Some("global-key")
        );
    }

    #[test]
    fn merge_inference_api_key_empty_when_no_global_inference_block() {
        let global = MurConfig {
            inference: None,
            ..MurConfig::default()
        };
        let project = MurConfig {
            inference: Some(InferenceConfig {
                provider: "anthropic".to_string(),
                model: "claude".to_string(),
                api_key: fake_key(&["sk-", "live-", "abc123"]),
                endpoint: String::new(),
            }),
            ..MurConfig::default()
        };

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(
            merged.inference.as_ref().map(|i| i.api_key.as_str()),
            Some("")
        );
        assert_eq!(
            merged.inference.as_ref().map(|i| i.model.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn merge_containment_takes_the_stronger_of_the_two_files() {
        use ContainmentClass::{Advisory, Scoped, Sealed};

        // Neither file declares anything: still nothing, not a defaulted advisory.
        assert_eq!(merge_containment(None, None), None);

        // One side only: that side's value survives untouched, from either slot.
        assert_eq!(merge_containment(Some(Scoped), None), Some(Scoped));
        assert_eq!(merge_containment(None, Some(Scoped)), Some(Scoped));

        // Both sides: the stronger wins regardless of which file holds it — the project file
        // may raise the global floor but must never lower it.
        assert_eq!(
            merge_containment(Some(Advisory), Some(Sealed)),
            Some(Sealed)
        );
        assert_eq!(
            merge_containment(Some(Sealed), Some(Advisory)),
            Some(Sealed)
        );
        assert_eq!(
            merge_containment(Some(Advisory), Some(Scoped)),
            Some(Scoped)
        );
        assert_eq!(
            merge_containment(Some(Scoped), Some(Advisory)),
            Some(Scoped)
        );
        assert_eq!(merge_containment(Some(Scoped), Some(Scoped)), Some(Scoped));
    }

    #[test]
    fn merge_mur_configs_raises_but_never_lowers_the_containment_floor() {
        let global = MurConfig {
            containment: Some(ContainmentClass::Sealed),
            ..MurConfig::default()
        };
        let project = MurConfig {
            containment: Some(ContainmentClass::Advisory),
            ..MurConfig::default()
        };

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(merged.containment, Some(ContainmentClass::Sealed));
    }

    #[test]
    fn containment_parses_from_and_survives_a_config_yaml_round_trip() {
        let parsed: MurConfig = serde_yaml::from_str("containment: scoped\n").unwrap();
        assert_eq!(parsed.containment, Some(ContainmentClass::Scoped));

        let rendered = serde_yaml::to_string(&parsed).unwrap();
        assert!(
            rendered.contains("containment: scoped"),
            "expected the wire name in {rendered}"
        );

        // Absent key stays absent on the way back out, so writing a config file never
        // silently pins a floor the operator did not ask for.
        let bare: MurConfig = serde_yaml::from_str("registry:\n  default: official\n").unwrap();
        assert_eq!(bare.containment, None);
        assert!(!serde_yaml::to_string(&bare)
            .unwrap()
            .contains("containment"));
    }

    #[test]
    fn is_literal_inference_api_key_detects_literal_vs_env_ref() {
        assert!(is_literal_inference_api_key(&fake_key(&[
            "sk-", "live-", "abc123"
        ])));
        assert!(!is_literal_inference_api_key("${SOME_VAR}"));
        assert!(!is_literal_inference_api_key(""));
    }

    #[test]
    fn load_effective_mur_config_identical_to_load_mur_config_when_no_project_file() {
        let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_home = env::var_os("HOME");
        let saved_cwd = env::current_dir().expect("cwd");

        let tmp_home = tempfile::tempdir().expect("failed to create temp HOME");
        let murmur_dir = tmp_home.path().join(".murmur");
        fs::create_dir_all(&murmur_dir).expect("failed to create .murmur dir");
        fs::write(
            murmur_dir.join("config.yaml"),
            "registry:\n  default: official\n",
        )
        .expect("failed to write config.yaml");

        let tmp_cwd = tempfile::tempdir().expect("failed to create temp cwd");

        // SAFETY: serialized by HOME_ENV_LOCK.
        unsafe {
            env::set_var("HOME", tmp_home.path());
        }
        env::set_current_dir(tmp_cwd.path()).expect("set cwd");

        let baseline = load_mur_config();
        let effective = load_effective_mur_config();

        // SAFETY: still holding `_guard`.
        unsafe {
            match &saved_home {
                Some(v) => env::set_var("HOME", v),
                None => env::remove_var("HOME"),
            }
        }
        let _ = env::set_current_dir(&saved_cwd);

        let baseline = baseline.expect("load_mur_config should succeed");
        let effective = effective.expect("load_effective_mur_config should succeed");
        assert_eq!(baseline.registry.default, effective.registry.default);
        assert_eq!(
            baseline.registry.sources.len(),
            effective.registry.sources.len()
        );
        assert_eq!(baseline.beta.enabled, effective.beta.enabled);
    }
}
