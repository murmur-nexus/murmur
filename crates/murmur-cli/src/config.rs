use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use murmur_artifact::ContainmentClass;
use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_CFG_004, E_CFG_005, E_IO_001, E_IO_003};

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
    ///
    /// `unconfined` is the operator's stance rather than a floor, and only the global file may
    /// say it: a project file saying it is refused with `E-CFG-005` at load, and the merged value
    /// never holds it. `mur run` reads it from [`ContainmentConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentClass>,
    /// Provider keys by credential name. A manifest's `gateway.api_key: ${NAME}` is answered from
    /// `credentials.NAME` here before the environment, and `mur run` re-reads the file while a
    /// capsule runs. Read from the global file only (see [`merge_mur_configs`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credentials: BTreeMap<String, String>,
    /// Machine-wide inference spend ceilings. Merged as a min, never a project-wins override (see
    /// [`merge_spend`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendConfig>,
    /// Host folders every contained capsule on this host may reach (see
    /// [`capsule_runtime::host_paths`]). Read from the global file only: a project file in which
    /// the key is present at all is refused by [`load_effective_mur_config_if_any_exists`]. `None`
    /// is the key absent; `Some(vec![])` is `host_paths: []`, which a project file may not hold
    /// either. Each entry is checked when a command uses it ([`launch_host_paths`]), so a malformed
    /// one names itself rather than failing the whole file's parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_paths: Option<Vec<capsule_runtime::host_paths::HostPathEntry>>,
}

/// The `spend:` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpendConfig {
    /// Most tokens — the runtime's own measured input plus output — every `mur run` on this
    /// `~/.murmur` may spend per UTC day, counted through the shared ledger under
    /// `~/.murmur/spend/`. Approximate: see the config reference. `0` is refused at load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_tokens_per_day: Option<u64>,
}

/// No file at all reads as this: no third-party source, which leaves only the built-in `murmur`
/// source (see [`crate::source::Sources`]).
impl Default for MurConfig {
    fn default() -> Self {
        Self {
            registry: RegistryConfig::default(),
            inference: None,
            beta: BetaConfig::default(),
            containment: None,
            credentials: BTreeMap::new(),
            spend: None,
            host_paths: None,
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
    /// The removed `default:` key, read so that building the sources can refuse it by name
    /// (`E-CFG-003`) while commands that never build them still load the file. Kept on a rewrite
    /// for the same reason.
    #[serde(default, rename = "default", skip_serializing_if = "Option::is_none")]
    pub removed_default: Option<serde_norway::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceConfig>,
    /// Override URL for the public artifact index fetched by `mur search`.
    /// Key: registry.index_url in ~/.murmur/config.yaml
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_url: Option<String>,
}

/// One `registry.sources` entry: the GitHub repository that serves its owner's scope.
///
/// `name`, `type` and `url` are the removed keys of the earlier source form, read and kept only so
/// that building the sources can refuse them by name (`E-CFG-003`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SourceConfig {
    /// `<owner>/<repo>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A GitHub token, written as `${VAR}`, `VAR` or the literal token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<serde_norway::Value>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub removed_type: Option<serde_norway::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<serde_norway::Value>,
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

/// Writes `~/.murmur/config.yaml` at `0600` inside a `~/.murmur` held at `0700`, whatever mode
/// either had before. Fails with `E-IO-003` rather than leave the file written wider.
pub fn save_mur_config(config: &MurConfig) -> Result<(), CliError> {
    write_mur_config_file(&mur_config_path()?, config, true)
}

/// Writes `<cwd>/.murmur/config.yaml` under the umask. It may not hold credentials.
pub fn save_project_mur_config(config: &MurConfig) -> Result<(), CliError> {
    write_mur_config_file(&project_mur_config_path()?, config, false)
}

/// Loads the effective `MurConfig`: the global (`~/.murmur/config.yaml`) merged with
/// the project-level (`<cwd>/.murmur/config.yaml`) file per-key (see `merge_mur_configs`).
/// When neither file exists, this is field-for-field identical to `load_mur_config()`.
pub fn load_effective_mur_config() -> Result<MurConfig, CliError> {
    load_effective_mur_config_if_any_exists().map(|opt| opt.unwrap_or_default())
}

/// Same as `load_effective_mur_config`, but returns `None` when neither the global nor
/// the project-level file exists (mirrors `load_mur_config_if_exists`'s None/Some contract),
/// for call sites that must not read the default's values as settings the operator chose.
pub fn load_effective_mur_config_if_any_exists() -> Result<Option<MurConfig>, CliError> {
    let global_opt = load_mur_config_if_exists()?;
    let project_opt = load_project_mur_config_if_exists()?;

    if global_opt.is_none() && project_opt.is_none() {
        return Ok(None);
    }

    if let Some(project) = &project_opt {
        refuse_project_unconfined(project)?;
        refuse_project_host_paths(&project_mur_config_path()?, project)?;
        warn_if_project_api_key_literal(project)?;
        if let Some(warning) = project_credentials_warning(&project_mur_config_path()?, project) {
            capsule_runtime::report_eprintln!("{warning}");
        }
    }

    let global = global_opt.unwrap_or_default();
    Ok(Some(merge_mur_configs(global, project_opt)))
}

/// `E-CFG-005` when the project file says `containment: unconfined`: only the operator's global
/// file may choose the unconfined stance.
fn refuse_project_unconfined(project: &MurConfig) -> Result<(), CliError> {
    if project.containment != Some(ContainmentClass::Unconfined) {
        return Ok(());
    }
    Err(CliError::with_hint(
        E_CFG_005,
        format!(
            "{} says containment: unconfined, which a project config cannot choose",
            project_mur_config_path()?.display()
        ),
        "only the operator chooses unconfined, with `mur run --unconfined` or `containment: \
         unconfined` in ~/.murmur/config.yaml; remove the key or name a floor (advisory, scoped, \
         sealed)",
    ))
}

/// The `containment` each config file states, unmerged: what `mur run`'s stance resolution
/// reads, because the global file may choose the unconfined stance and the project file may not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContainmentConfig {
    /// `containment` in `~/.murmur/config.yaml`, `unconfined` included.
    pub global: Option<ContainmentClass>,
    /// `containment` in `<cwd>/.murmur/config.yaml`, never `unconfined`.
    pub project: Option<ContainmentClass>,
}

/// Reads [`ContainmentConfig`] from both files, refusing a project file that says
/// `containment: unconfined` with `E-CFG-005`.
pub fn load_containment_config() -> Result<ContainmentConfig, CliError> {
    let global = load_mur_config_if_exists()?.and_then(|config| config.containment);
    let project = match load_project_mur_config_if_exists()? {
        Some(project) => {
            refuse_project_unconfined(&project)?;
            project.containment
        }
        None => None,
    };
    Ok(ContainmentConfig { global, project })
}

/// Per-key merge of a global and an optional project-level `MurConfig`. `project` values win
/// for non-empty scalars; `registry.sources` and `beta.enabled` merge as unions; `api_key` and
/// `credentials` are unconditionally sourced from `global`, never `project`, regardless of their
/// shape. Returns `global` when `project` is `None`, with an `unconfined` containment dropped as
/// [`merge_containment`] drops it.
pub fn merge_mur_configs(global: MurConfig, project: Option<MurConfig>) -> MurConfig {
    let Some(project) = project else {
        return MurConfig {
            containment: merge_containment(global.containment, None),
            ..global
        };
    };

    MurConfig {
        registry: RegistryConfig {
            removed_default: project
                .registry
                .removed_default
                .or(global.registry.removed_default),
            sources: merge_sources(global.registry.sources, project.registry.sources),
            index_url: pick_non_empty(project.registry.index_url, global.registry.index_url),
        },
        inference: merge_inference(global.inference, project.inference),
        beta: BetaConfig {
            enabled: merge_beta_enabled(global.beta.enabled, project.beta.enabled),
        },
        containment: merge_containment(global.containment, project.containment),
        credentials: global.credentials,
        spend: merge_spend(global.spend, project.spend),
        host_paths: global.host_paths,
    }
}

/// `E-CFG-004` when the project-level file at `path` holds a `host_paths` key at all, an empty
/// list included: only the operator's global config may lend a contained capsule a host folder.
pub(crate) fn refuse_project_host_paths(path: &Path, project: &MurConfig) -> Result<(), CliError> {
    if project.host_paths.is_none() {
        return Ok(());
    }
    Err(CliError::with_hint(
        E_CFG_004,
        format!(
            "{} sets host_paths:, but host_paths is read from the global config \
             (~/.murmur/config.yaml) only",
            path.display()
        ),
        format!(
            "remove host_paths from {}; an operator lends a host folder in ~/.murmur/config.yaml \
             — see {}",
            path.display(),
            murmur_artifact::diagnostic_link("E-CFG-004")
        ),
    ))
}

/// The base `~` in a `host_paths` entry expands against: the home [`mur_config_path`] finds the
/// global config in, or `None` when `HOME` (and `USERPROFILE`) is unset or empty.
fn host_paths_home() -> Option<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())?;
    absolute_home(home.into()).ok()
}

/// The global config's `host_paths`, shape-checked and `~` expanded (`E-CFG-004` naming the
/// entry). Empty without the key.
pub(crate) fn declared_host_paths(
    config: Option<&MurConfig>,
) -> Result<Vec<capsule_runtime::host_paths::DeclaredHostPath>, CliError> {
    let Some(entries) = config.and_then(|config| config.host_paths.as_deref()) else {
        return Ok(Vec::new());
    };
    capsule_runtime::host_paths::declare_host_paths(entries, host_paths_home().as_deref())
        .map_err(|refusal| crate::error::host_path_refusal_to_cli(&refusal))
}

/// The global config's `host_paths`, checked on this host for a session whose work folder root
/// is `workdir`, as `mur run` grants them: every launch refusal is `E-CFG-004` naming the entry.
/// What `mur run`, `mur run --explain-scope` and `mur doctor` call.
pub(crate) fn launch_host_paths(
    config: Option<&MurConfig>,
    workdir: &Path,
) -> Result<Vec<capsule_runtime::host_paths::HostPath>, CliError> {
    let declared = declared_host_paths(config)?;
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    let home = host_paths_home();
    let murmur_home = home.as_ref().map(|home| home.join(".murmur"));
    capsule_runtime::host_paths::resolve_host_paths(
        &declared,
        &capsule_runtime::host_paths::HostPathContext {
            home: home.as_deref(),
            murmur_home: murmur_home.as_deref(),
            workdir,
        },
    )
    .map_err(|refusal| crate::error::host_path_refusal_to_cli(&refusal))
}

/// Merges the two `spend` sections, keeping the **lower** `machine_tokens_per_day`, deliberately
/// breaking this file's project-wins rule: a ceiling is a limit, so a project file may lower what
/// the global file set but never raise it. A side that sets nothing does not participate.
pub fn merge_spend(
    global: Option<SpendConfig>,
    project: Option<SpendConfig>,
) -> Option<SpendConfig> {
    let global = global.and_then(|spend| spend.machine_tokens_per_day);
    let project = project.and_then(|spend| spend.machine_tokens_per_day);
    let machine_tokens_per_day = match (global, project) {
        (Some(global), Some(project)) => Some(global.min(project)),
        (value, None) | (None, value) => value,
    };
    machine_tokens_per_day.map(|value| SpendConfig {
        machine_tokens_per_day: Some(value),
    })
}

/// Merges the two `containment` declarations as a **max**, deliberately breaking this file's
/// project-wins rule: a containment class is a floor, so a project file must be able to raise
/// what the global file asked for but never to lower it. `None` on a side means that side asked
/// for nothing and does not participate; `None` on both stays `None` (no declaration at all,
/// which is distinct from an explicit `advisory`).
///
/// `unconfined` never takes part: it is the operator's stance, not a floor, so the merged value is
/// always a declarable floor or `None`.
pub fn merge_containment(
    global: Option<ContainmentClass>,
    project: Option<ContainmentClass>,
) -> Option<ContainmentClass> {
    let declarable = |class: Option<ContainmentClass>| {
        class.filter(|class| *class != ContainmentClass::Unconfined)
    };
    match (declarable(global), declarable(project)) {
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

/// Union of `registry.sources` by scope: a project entry replaces the global entry for the same
/// scope, a project entry for a new scope is appended, and every global entry the project does not
/// replace is kept. An entry naming no usable repository has no scope and is kept as it is, for
/// [`crate::source::Sources`] to refuse.
fn merge_sources(global: Vec<SourceConfig>, project: Vec<SourceConfig>) -> Vec<SourceConfig> {
    let mut merged = global;
    for source in project {
        let scope = source_scope(&source);
        match merged
            .iter_mut()
            .find(|existing| scope.is_some() && source_scope(existing) == scope)
        {
            Some(existing) => *existing = source,
            None => merged.push(source),
        }
    }
    merged
}

/// The scope `source` serves, or `None` when its `repo` is no `<owner>/<repo>` with a usable owner.
pub(crate) fn source_scope(source: &SourceConfig) -> Option<String> {
    let (owner, repo) = crate::source::split_github_repo(source.repo.as_deref()?.trim())?;
    crate::source::scope_of_github_repo(owner, repo).ok()
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
    capsule_runtime::report_eprintln!(
        "warning: {} sets inference.api_key to a literal value, but inference.api_key is \
         always read from the global config (~/.murmur/config.yaml); this project-level value \
         will be ignored",
        path.display()
    );
    Ok(())
}

/// The warning printed when the project-level file at `path` holds a non-empty `credentials:` map,
/// which nothing reads. Names the file, never a value.
fn project_credentials_warning(path: &Path, project: &MurConfig) -> Option<String> {
    (!project.credentials.is_empty()).then(|| {
        format!(
            "warning: {} sets credentials:, but credentials are read from the global config \
             (~/.murmur/config.yaml) only; these project-level entries will be ignored",
            path.display()
        )
    })
}

/// Whether `name` can be a credential name: the `${NAME}` grammar a manifest's `gateway.api_key`
/// accepts, `[A-Z_][A-Z0-9_]*`.
pub(crate) fn is_valid_credential_name(name: &str) -> bool {
    is_valid_env_variable(name)
}

/// True when `value` is non-empty and is not a `${VAR}` env-var reference — i.e. it would be
/// silently ignored (and, on read, warned about) as a project-level `inference.api_key`.
pub(crate) fn is_literal_inference_api_key(value: &str) -> bool {
    !value.is_empty() && parse_env_reference(value).is_none()
}

pub(crate) fn read_mur_config_file(config_path: &Path) -> Result<Option<MurConfig>, CliError> {
    if !config_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(config_path).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {source}", config_path.display()),
        )
    })?;

    let config = serde_norway::from_str::<MurConfig>(&raw).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to parse {}: {source}", config_path.display()),
        )
    })?;
    if config
        .spend
        .as_ref()
        .and_then(|spend| spend.machine_tokens_per_day)
        == Some(0)
    {
        return Err(CliError::new(
            E_IO_003,
            format!(
                "invalid spend.machine_tokens_per_day 0 in {} (expected a positive integer)",
                config_path.display()
            ),
        ));
    }
    Ok(Some(config))
}

/// Replaces `config_path` by rename, so the inode changes on every write and a running capsule's
/// credential stamp sees it.
///
/// `private` holds the parent directory at `0700` and the file at `0600`, reapplied on every
/// write; otherwise both are left to the umask.
fn write_mur_config_file(
    config_path: &Path,
    config: &MurConfig,
    private: bool,
) -> Result<(), CliError> {
    if private {
        let serialized = serde_norway::to_string(config).map_err(|source| {
            CliError::new(E_IO_003, format!("failed to serialize config: {source}"))
        })?;
        if let Some(parent) = config_path.parent() {
            capsule_runtime::murmur_home::hold_private_dir(parent).map_err(|reason| {
                CliError::new(
                    E_IO_003,
                    format!("failed to hold {} owner-only: {reason}", parent.display()),
                )
            })?;
        }
        return capsule_runtime::murmur_home::write_private_file(
            config_path,
            serialized.as_bytes(),
        )
        .map_err(|reason| {
            CliError::new(
                E_IO_003,
                format!("failed to write {}: {reason}", config_path.display()),
            )
        });
    }

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to create {}: {source}", parent.display()),
            )
        })?;
    }

    let serialized = serde_norway::to_string(config).map_err(|source| {
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

pub fn mur_config_path() -> Result<PathBuf, CliError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| CliError::new(E_IO_001, "could not determine home directory"))?;
    Ok(absolute_home(home.into())?
        .join(".murmur")
        .join("config.yaml"))
}

/// `home` made absolute against the working directory, as [`mur_config_path`] and
/// [`host_paths_home`] both read it.
fn absolute_home(home: PathBuf) -> Result<PathBuf, CliError> {
    if home.is_absolute() {
        return Ok(home);
    }
    env::current_dir()
        .map(|cwd| cwd.join(home))
        .map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to determine current working directory: {source}"),
            )
        })
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

/// Test helpers shared by every test in the crate that points `HOME` or the working directory at a
/// scratch directory, or checks a file mode.
#[cfg(test)]
pub(crate) mod test_env {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::HOME_ENV_LOCK;

    /// Holds `HOME` and the working directory at scratch paths until dropped, serialised by
    /// [`HOME_ENV_LOCK`].
    pub(crate) struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_home: Option<std::ffi::OsString>,
        saved_cwd: PathBuf,
    }

    impl EnvGuard {
        pub(crate) fn set_up(home: &Path, cwd: &Path) -> Self {
            let lock = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved_home = std::env::var_os("HOME");
            let saved_cwd = std::env::current_dir().expect("cwd");
            // SAFETY: serialized by HOME_ENV_LOCK across every test in the crate that sets HOME.
            unsafe {
                std::env::set_var("HOME", home);
            }
            std::env::set_current_dir(cwd).expect("set cwd");
            Self {
                _lock: lock,
                saved_home,
                saved_cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: still holding `_lock`.
            unsafe {
                match &self.saved_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
            let _ = std::env::set_current_dir(&self.saved_cwd);
        }
    }

    /// `path`'s permission bits.
    pub(crate) fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    pub(crate) fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
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
    fn no_config_file_names_no_third_party_source() {
        let config = MurConfig::default();
        assert!(config.registry.sources.is_empty());
        assert!(config.registry.removed_default.is_none());
        let rendered = serde_norway::to_string(&config).unwrap();
        assert!(!rendered.contains("sources"), "{rendered}");
    }

    /// The removed keys are read rather than refused here: only building the sources refuses
    /// them, so a command that never builds them still loads the file, and rewriting the file
    /// keeps them for that refusal.
    #[test]
    fn a_config_file_with_removed_registry_keys_still_loads_and_keeps_them() {
        let _guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_home = env::var_os("HOME");

        let tmp_home = tempfile::tempdir().expect("failed to create temp HOME");
        let murmur_dir = tmp_home.path().join(".murmur");
        fs::create_dir_all(&murmur_dir).expect("failed to create .murmur dir");
        fs::write(
            murmur_dir.join("config.yaml"),
            "registry:\n  default: official\n  sources:\n    - name: official\n      type: github\n      repo: some-user/custom-artifacts\n      url: https://example.com\n",
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
        let source = &config.registry.sources[0];
        assert_eq!(source.repo.as_deref(), Some("some-user/custom-artifacts"));
        assert!(config.registry.removed_default.is_some());
        assert!(source.name.is_some() && source.removed_type.is_some() && source.url.is_some());

        let rendered = serde_norway::to_string(&config).unwrap();
        for key in [
            "default: official",
            "name: official",
            "type: github",
            "url: ",
        ] {
            assert!(rendered.contains(key), "{key} missing from {rendered}");
        }
    }

    fn source(repo: &str) -> SourceConfig {
        SourceConfig {
            repo: Some(repo.to_string()),
            ..SourceConfig::default()
        }
    }

    fn repos(config: &MurConfig) -> Vec<&str> {
        config
            .registry
            .sources
            .iter()
            .map(|source| source.repo.as_deref().unwrap_or("-"))
            .collect()
    }

    #[test]
    fn merge_with_no_project_config_returns_global_unchanged() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("ana/a")];
        let merged = merge_mur_configs(global.clone(), None);
        assert_eq!(repos(&merged), ["ana/a"]);
        assert_eq!(merged.beta.enabled, global.beta.enabled);
    }

    #[test]
    fn merge_sources_appends_a_project_source_for_a_new_scope() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("ana/a")];
        let mut project = MurConfig::default();
        project.registry.sources = vec![source("bob/b")];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(repos(&merged), ["ana/a", "bob/b"]);
    }

    #[test]
    fn merge_sources_replaces_the_global_source_of_the_same_scope() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("ana/a"), source("bob/b")];
        let mut project = MurConfig::default();
        project.registry.sources = vec![source("Ana/c")];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(repos(&merged), ["Ana/c", "bob/b"]);
    }

    /// An entry with no usable repository has no scope, so it replaces nothing and is kept for
    /// building the sources to refuse.
    #[test]
    fn merge_sources_keeps_an_entry_with_no_scope() {
        let mut global = MurConfig::default();
        global.registry.sources = vec![source("ana/a")];
        let mut project = MurConfig::default();
        project.registry.sources = vec![SourceConfig::default(), source("ana")];

        let merged = merge_mur_configs(global, Some(project));

        assert_eq!(repos(&merged), ["ana/a", "-", "ana"]);
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
    fn spend_config_parses_and_survives_a_config_yaml_round_trip() {
        let parsed: MurConfig =
            serde_norway::from_str("spend:\n  machine_tokens_per_day: 2000000\n").unwrap();
        assert_eq!(
            parsed.spend,
            Some(SpendConfig {
                machine_tokens_per_day: Some(2_000_000)
            })
        );
        let rendered = serde_norway::to_string(&parsed).unwrap();
        assert!(
            rendered.contains("machine_tokens_per_day: 2000000"),
            "{rendered}"
        );
        let bare: MurConfig = serde_norway::from_str("{}").unwrap();
        assert_eq!(bare.spend, None);
        assert!(!serde_norway::to_string(&bare).unwrap().contains("spend"));
    }

    #[test]
    fn spend_config_zero_is_refused_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "spend:\n  machine_tokens_per_day: 0\n").unwrap();
        let err = read_mur_config_file(&path).unwrap_err();
        assert_eq!(err.code, E_IO_003);
        assert!(
            err.message.contains("spend.machine_tokens_per_day"),
            "{}",
            err.message
        );
    }

    #[test]
    fn spend_config_merge_keeps_the_lower_ceiling() {
        let set = |n| {
            Some(SpendConfig {
                machine_tokens_per_day: Some(n),
            })
        };
        assert_eq!(merge_spend(None, None), None);
        assert_eq!(merge_spend(set(10), None), set(10));
        assert_eq!(merge_spend(None, set(10)), set(10));
        assert_eq!(merge_spend(set(10), set(20)), set(10));
        assert_eq!(merge_spend(set(20), set(10)), set(10));
        assert_eq!(merge_spend(Some(SpendConfig::default()), set(7)), set(7));
        assert_eq!(merge_spend(Some(SpendConfig::default()), None), None);
    }

    #[test]
    fn spend_config_merge_mur_configs_lowers_but_never_raises_the_ceiling() {
        let global = MurConfig {
            spend: Some(SpendConfig {
                machine_tokens_per_day: Some(1_000),
            }),
            ..MurConfig::default()
        };
        let raising = MurConfig {
            spend: Some(SpendConfig {
                machine_tokens_per_day: Some(5_000),
            }),
            ..MurConfig::default()
        };
        let merged = merge_mur_configs(global.clone(), Some(raising));
        assert_eq!(merged.spend.unwrap().machine_tokens_per_day, Some(1_000));
        let lowering = MurConfig {
            spend: Some(SpendConfig {
                machine_tokens_per_day: Some(10),
            }),
            ..MurConfig::default()
        };
        let merged = merge_mur_configs(global, Some(lowering));
        assert_eq!(merged.spend.unwrap().machine_tokens_per_day, Some(10));
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
        let parsed: MurConfig = serde_norway::from_str("containment: scoped\n").unwrap();
        assert_eq!(parsed.containment, Some(ContainmentClass::Scoped));

        let rendered = serde_norway::to_string(&parsed).unwrap();
        assert!(
            rendered.contains("containment: scoped"),
            "expected the wire name in {rendered}"
        );

        // Absent key stays absent on the way back out, so writing a config file never
        // silently pins a floor the operator did not ask for.
        let bare: MurConfig = serde_norway::from_str("registry:\n  index_url: x\n").unwrap();
        assert_eq!(bare.containment, None);
        assert!(!serde_norway::to_string(&bare)
            .unwrap()
            .contains("containment"));
    }

    /// `unconfined` never takes part in the merge: the merged value is always a declarable floor
    /// or nothing.
    #[test]
    fn merge_containment_never_lets_unconfined_take_part() {
        use ContainmentClass::{Advisory, Scoped, Sealed, Unconfined};
        for other in [None, Some(Advisory), Some(Scoped), Some(Sealed)] {
            assert_eq!(merge_containment(Some(Unconfined), other), other);
            assert_eq!(merge_containment(other, Some(Unconfined)), other);
        }
        let global = MurConfig {
            containment: Some(Unconfined),
            ..MurConfig::default()
        };
        assert_eq!(merge_mur_configs(global.clone(), None).containment, None);
        assert_eq!(
            merge_mur_configs(global, Some(MurConfig::default())).containment,
            None
        );
    }

    /// The global file may say `containment: unconfined`, and it survives a save and a load.
    #[test]
    fn a_global_unconfined_containment_round_trips_through_save_mur_config() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _env = test_env::EnvGuard::set_up(home.path(), cwd.path());
        save_mur_config(&MurConfig {
            containment: Some(ContainmentClass::Unconfined),
            ..MurConfig::default()
        })
        .unwrap();
        let written = std::fs::read_to_string(mur_config_path().unwrap()).unwrap();
        assert!(written.contains("containment: unconfined"), "{written}");
        assert_eq!(
            load_mur_config().unwrap().containment,
            Some(ContainmentClass::Unconfined)
        );
        assert_eq!(
            load_containment_config().unwrap(),
            ContainmentConfig {
                global: Some(ContainmentClass::Unconfined),
                project: None,
            }
        );
        // Every command reading the effective config sees no floor from it.
        assert_eq!(load_effective_mur_config().unwrap().containment, None);
    }

    /// A project file saying `containment: unconfined` is `E-CFG-005`, naming the file, from
    /// every reader of the effective config and from the stance's own reader.
    #[test]
    fn a_project_unconfined_containment_is_e_cfg_005_naming_the_file() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _env = test_env::EnvGuard::set_up(home.path(), cwd.path());
        std::fs::create_dir_all(cwd.path().join(".murmur")).unwrap();
        std::fs::write(
            cwd.path().join(".murmur/config.yaml"),
            "containment: unconfined\n",
        )
        .unwrap();
        let path = project_mur_config_path().unwrap();
        for error in [
            load_effective_mur_config_if_any_exists().unwrap_err(),
            load_containment_config().unwrap_err(),
        ] {
            assert_eq!(error.code, E_CFG_005);
            assert!(
                error.message.contains(&path.display().to_string()),
                "{error}"
            );
            assert!(
                error
                    .hint
                    .as_deref()
                    .unwrap()
                    .contains("mur run --unconfined"),
                "{error}"
            );
        }
    }

    #[test]
    fn credentials_come_from_the_global_config_only() {
        let mut global = MurConfig::default();
        global
            .credentials
            .insert("PROVIDER_KEY".to_string(), "global-value".to_string());
        let mut project = MurConfig::default();
        project
            .credentials
            .insert("PROVIDER_KEY".to_string(), "project-value".to_string());
        project
            .credentials
            .insert("OTHER_KEY".to_string(), "project-other".to_string());

        let path = Path::new("/work/.murmur/config.yaml");
        let warning = project_credentials_warning(path, &project).expect("a warning");
        assert!(warning.contains("/work/.murmur/config.yaml"), "{warning}");
        assert!(!warning.contains("project-value"), "{warning}");
        assert!(project_credentials_warning(path, &MurConfig::default()).is_none());

        let merged = merge_mur_configs(global.clone(), Some(project));
        assert_eq!(merged.credentials, global.credentials);
    }

    #[test]
    fn credentials_empty_map_is_not_serialized() {
        let rendered = serde_norway::to_string(&MurConfig::default()).unwrap();
        assert!(!rendered.contains("credentials"), "{rendered}");

        let parsed: MurConfig =
            serde_norway::from_str("credentials:\n  PROVIDER_KEY: some-value\n").unwrap();
        assert_eq!(
            parsed.credentials.get("PROVIDER_KEY").map(String::as_str),
            Some("some-value")
        );
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
            "registry:\n  index_url: https://example.com/index.json\n",
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
        assert_eq!(baseline.registry.index_url, effective.registry.index_url);
        assert_eq!(
            baseline.registry.sources.len(),
            effective.registry.sources.len()
        );
        assert_eq!(baseline.beta.enabled, effective.beta.enabled);
    }

    /// A scratch directory outside `/tmp`, which a host path may not be under, and its canonical
    /// path: the working directory and `HOME` are compared as the process sees them.
    fn host_paths_scratch() -> (tempfile::TempDir, PathBuf) {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let base = if target.is_dir() {
            target
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        };
        let dir = tempfile::tempdir_in(base).unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        (dir, canonical)
    }

    const HOST_PATHS_YAML: &str = "host_paths:\n  - path: ~/.cargo\n    access: read-write\n  \
                                   - path: ~/data\n    access: read\n";

    #[test]
    fn host_paths_parse_expand_against_home_and_survive_a_round_trip() {
        let (_home_dir, home) = host_paths_scratch();
        let (_cwd_dir, cwd) = host_paths_scratch();
        let _env = test_env::EnvGuard::set_up(&home, &cwd);
        std::fs::create_dir_all(home.join(".murmur")).unwrap();
        std::fs::write(home.join(".murmur/config.yaml"), HOST_PATHS_YAML).unwrap();

        let config = load_mur_config().unwrap();
        let entries = config.host_paths.clone().unwrap();
        assert_eq!(entries.len(), 2);
        let declared = declared_host_paths(Some(&config)).unwrap();
        assert_eq!(declared[0].expanded, home.join(".cargo"));
        assert_eq!(
            declared[0].access,
            capsule_runtime::host_paths::HostPathAccess::ReadWrite
        );
        assert_eq!(declared[1].expanded, home.join("data"));

        save_mur_config(&config).unwrap();
        let reread = load_mur_config().unwrap();
        assert_eq!(reread.host_paths, Some(entries));
    }

    #[test]
    fn host_paths_merge_keeps_the_global_entries_and_never_the_projects() {
        let global = MurConfig {
            host_paths: Some(vec![capsule_runtime::host_paths::HostPathEntry {
                path: Some("/srv/global".to_string()),
                access: Some("read".to_string()),
            }]),
            ..MurConfig::default()
        };
        let project = MurConfig {
            host_paths: Some(vec![capsule_runtime::host_paths::HostPathEntry {
                path: Some("/srv/project".to_string()),
                access: Some("read-write".to_string()),
            }]),
            ..MurConfig::default()
        };
        let merged = merge_mur_configs(global.clone(), Some(project));
        assert_eq!(merged.host_paths, global.host_paths);
        let merged = merge_mur_configs(
            MurConfig::default(),
            Some(MurConfig {
                host_paths: Some(Vec::new()),
                ..MurConfig::default()
            }),
        );
        assert_eq!(merged.host_paths, None);
    }

    #[test]
    fn host_paths_malformed_entries_are_refused_with_e_cfg_004_naming_the_entry() {
        let (_home_dir, home) = host_paths_scratch();
        let (_cwd_dir, cwd) = host_paths_scratch();
        let _env = test_env::EnvGuard::set_up(&home, &cwd);
        let cases = [
            (
                "  - path: relative/dir\n    access: read\n",
                "host_paths[0] (relative/dir)",
            ),
            (
                "  - path: ~user/x\n    access: read\n",
                "host_paths[0] (~user/x)",
            ),
            ("  - path: /\n    access: read\n", "host_paths[0] (/)"),
            (
                "  - path: /home/..\n    access: read\n",
                "host_paths[0] (/home/..)",
            ),
            ("  - path: /srv\n", "access is required"),
            ("  - path: /srv\n    access: write\n", "access 'write'"),
            (
                "  - path: /srv\n    access: read\n  - path: /srv\n    access: read\n",
                "host_paths[1] (/srv)",
            ),
        ];
        for (entries, expected) in cases {
            let config: MurConfig =
                serde_norway::from_str(&format!("host_paths:\n{entries}")).unwrap();
            let error = declared_host_paths(Some(&config)).unwrap_err();
            assert_eq!(error.code, E_CFG_004, "{entries}");
            assert!(error.message.contains(expected), "{}", error.message);
        }
    }

    #[test]
    fn host_paths_in_a_project_config_are_refused_even_empty() {
        for entries in [
            "host_paths:\n  - path: /srv\n    access: read\n",
            "host_paths: []\n",
        ] {
            let (_home_dir, home) = host_paths_scratch();
            let (_cwd_dir, cwd) = host_paths_scratch();
            let _env = test_env::EnvGuard::set_up(&home, &cwd);
            std::fs::create_dir_all(cwd.join(".murmur")).unwrap();
            std::fs::write(cwd.join(".murmur/config.yaml"), entries).unwrap();
            let error = load_effective_mur_config_if_any_exists().unwrap_err();
            assert_eq!(error.code, E_CFG_004);
            assert!(
                error
                    .message
                    .contains(&cwd.join(".murmur/config.yaml").display().to_string()),
                "{}",
                error.message
            );
            assert!(
                error.message.contains("~/.murmur/config.yaml) only"),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn host_paths_absent_from_a_file_are_absent_from_its_rewrite() {
        let serialized = serde_norway::to_string(&MurConfig::default()).unwrap();
        assert!(!serialized.contains("host_paths"), "{serialized}");
    }
}
