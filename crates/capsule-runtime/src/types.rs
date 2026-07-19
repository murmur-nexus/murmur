use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use murmur_artifact::{
    ArtifactImplementation, ArtifactRuntime, ContextConfig, HookConfig, InferenceConfig,
    LifecycleConfig, LifecycleOverride, Registry,
};
use wasmtime::{component::Component, Engine};

use crate::{
    bindings::host::murmur::tool::run::ToolResult,
    hooks::ShellDispatchInfo,
    limits::{EpochTicker, ExecutionLimits},
};

pub(crate) struct DispatchOutcome {
    pub result: ToolResult,
    pub shell: Option<ShellDispatchInfo>,
    /// True when the dispatch was served by reading a skill.md file rather than running
    /// a binary or WASM component. Used by the agent loop to write a `skill_call` trace
    /// event instead of a `tool_call` event.
    pub is_skill: bool,
}

impl DispatchOutcome {
    pub(crate) fn tool(result: ToolResult) -> Self {
        Self {
            result,
            shell: None,
            is_skill: false,
        }
    }

    pub(crate) fn skill(result: ToolResult) -> Self {
        Self {
            result,
            shell: None,
            is_skill: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRequest {
    pub name: String,
    pub version: String,
    pub runtime: ArtifactRuntime,
    /// Optional local source path (skill artifacts only). When set, the runtime resolves the
    /// skill's `skill.md` directly from this path and skips registry resolution entirely.
    /// Relative paths resolve against `StageRequest::manifest_dir`.
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockExpectation {
    pub name: String,
    pub resolved_version: String,
    pub sha256_wasm: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLockArtifact {
    pub name: String,
    pub resolved_version: String,
    pub sha256_wasm: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilityPolicy {
    pub network_allow: Vec<String>,
    pub filesystem_scope: Option<String>,
    pub shell_allow: Vec<String>,
    pub spawn_allow: Vec<String>,
    pub shell_strip_env: Vec<String>,
    pub shell_baseline_env: Vec<String>,
    /// Host env var names a WASM guest may observe, from `capabilities.env.allow`. Empty by
    /// default: a guest sees only the runtime's own `MURMUR_*` injection unless the manifest
    /// names a variable here. `shell_strip_env` and the credential patterns still apply on
    /// top, so a credential-shaped name declared here is stripped anyway.
    pub env_allow: Vec<String>,
    /// Execution limits applied to every guest store in the session. Always fully
    /// resolved — `capability_policy_from_runtime_manifest` substitutes a default for
    /// each field `capabilities.limits` omits, so there is no "unset" state here and a
    /// silent manifest yields [`ExecutionLimits::default`] rather than no limits.
    pub limits: ExecutionLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledArtifactSummary {
    pub name: String,
    pub version: String,
    pub runtime: ArtifactRuntime,
    pub implementation: Option<ArtifactImplementation>,
}

#[derive(Clone)]
pub(crate) struct StagedHookArtifact {
    pub name: String,
    pub version: String,
    pub component: Component,
    pub config: HookConfig,
}

#[derive(Debug, Clone)]
pub struct StageRequest {
    pub manifest_dir: PathBuf,
    pub capsule_name: String,
    pub capsule_version: String,
    pub capsule_component_bytes: Vec<u8>,
    pub artifacts: Vec<ArtifactRequest>,
    pub allowlisted_tools: HashSet<String>,
    pub lock_expectations: Option<Vec<LockExpectation>>,
    pub capability_policy: CapabilityPolicy,
    pub inference: Option<InferenceConfig>,
    pub context: Option<ContextConfig>,
    /// OTLP/HTTP endpoint for span export; None = no external OTel emission.
    pub otel_endpoint: Option<String>,
    /// JSON-serialized EvalConfig injected into hook WASI env as MURMUR_EVAL_CONFIG.
    pub eval_config_json: Option<String>,
    /// Dataset case identifier injected as MURMUR_CASE_ID when running a dataset.
    pub case_id: Option<String>,
    /// Dataset identifier injected as MURMUR_DATASET_ID when running a dataset.
    pub dataset_id: Option<String>,
    /// Lifecycle config from the manifest (None = use LifecycleConfig::default()).
    pub lifecycle: Option<LifecycleConfig>,
    /// Optional override applied on top of lifecycle (e.g. from CLI flags or mur-roost).
    pub lifecycle_override: Option<LifecycleOverride>,
    /// Trace config from the manifest (None = defaults apply: input captured, output not).
    pub trace: Option<murmur_artifact::TraceConfig>,
    /// Optional user project directory to mount as the capsule's accessible workspace.
    /// When set, session artifacts go into `<workdir>/.murmur/<session_id>/`.
    /// When None, uses the default temp dir (existing behavior).
    pub workdir: Option<std::path::PathBuf>,
    /// Address to bind the HTTP server on. Defaults to "127.0.0.1" for local runs.
    /// Use "0.0.0.0" when deploying to a VM so the capsule is reachable externally.
    pub bind_addr: String,
    /// Internal port from `network.internal_port` in the manifest. When `Some`, the runtime
    /// binds strictly to this port and errors if it is already in use. When `None`, the OS
    /// assigns a port.
    pub internal_port: Option<u16>,
    /// Job ID assigned by mur-roost for this capsule instance. Injected as MURMUR_JOB_ID so
    /// spawned child capsules can set `spawned_by` when calling POST /spawn on mur-roost.
    /// None when not launched via mur-roost (direct CLI, tests).
    pub job_id: Option<String>,
}

pub struct StagedSession {
    pub session_id: String,
    pub workdir: PathBuf,
    /// User project directory (or session dir if no --workdir). WASM tools see this at ".".
    pub accessible_workdir: std::path::PathBuf,
    pub(crate) manifest_dir: PathBuf,
    pub(crate) capsule_name: String,
    pub(crate) capsule_version: String,
    pub(crate) capsule_url: String,
    pub resolved_lock_artifacts: Vec<ResolvedLockArtifact>,
    pub(crate) installed_artifacts: Vec<InstalledArtifactSummary>,
    pub(crate) inference: Option<InferenceConfig>,
    pub(crate) context: Option<ContextConfig>,
    pub(crate) engine: Engine,
    /// `None` for manifest-only agent capsules; `Some` for script capsules with a WASM component.
    pub(crate) capsule_component: Option<Component>,
    pub(crate) tool_components: HashMap<String, Component>,
    pub(crate) hook_components: Vec<StagedHookArtifact>,
    pub(crate) allowlisted_tools: HashSet<String>,
    pub(crate) capability_policy: CapabilityPolicy,
    pub(crate) otel_endpoint: Option<String>,
    pub(crate) eval_config_json: Option<String>,
    pub(crate) case_id: Option<String>,
    pub(crate) dataset_id: Option<String>,
    /// Resolved lifecycle config (manifest + override already applied).
    pub(crate) lifecycle: LifecycleConfig,
    /// Whether to capture tool output in trace events (resolved from manifest trace config).
    pub(crate) trace_include_tool_output: bool,
    /// Address the HTTP server is bound to (copied from StageRequest::bind_addr).
    pub(crate) bind_addr: String,
    /// Internal port from the manifest (copied from StageRequest::internal_port).
    pub(crate) internal_port: Option<u16>,
    /// Job ID from mur-roost (copied from StageRequest::job_id).
    pub(crate) job_id: Option<String>,
    /// Registry used to resolve this session's artifacts, retained so `manage.pull()` can
    /// resolve additional artifacts at runtime after staging has completed.
    pub(crate) registry: Arc<dyn Registry>,
    /// Keeps this session's epoch ticker running: without it no epoch deadline set on any
    /// store built from `engine` can ever fire. Held here (rather than detached) so ticking
    /// stops when the session's `StagedSession` drops. `launch_session` moves the other
    /// fields out one by one, which leaves this one in place until the session returns.
    pub(crate) _epoch_ticker: EpochTicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResult {
    pub session_id: String,
    pub workdir: PathBuf,
}

pub fn capability_policy_from_runtime_manifest(
    manifest: &murmur_artifact::RuntimeManifest,
) -> CapabilityPolicy {
    let caps = manifest.capabilities.as_ref();
    let network_allow = caps
        .and_then(|c| c.network.as_ref())
        .map(|network| network.allow.clone())
        .unwrap_or_default();

    let filesystem_scope = caps
        .and_then(|c| c.filesystem.as_ref())
        .and_then(|filesystem| filesystem.scope.clone());

    let shell_allow = caps
        .and_then(|c| c.shell.as_ref())
        .map(|shell| shell.allow.clone())
        .unwrap_or_default();
    let shell_strip_env = caps
        .and_then(|c| c.shell.as_ref())
        .and_then(|shell| shell.strip_env.clone())
        .unwrap_or_default();
    let shell_baseline_env = caps
        .and_then(|c| c.shell.as_ref())
        .and_then(|shell| shell.baseline_env.clone())
        .unwrap_or_default();

    let spawn_allow = caps
        .and_then(|c| c.spawn.as_ref())
        .map(|spawn| spawn.allow.clone())
        .unwrap_or_default();

    let env_allow = caps
        .and_then(|c| c.env.as_ref())
        .map(|env| env.allow.clone())
        .unwrap_or_default();

    let limits = ExecutionLimits::resolve(caps.and_then(|c| c.limits.as_ref()));

    CapabilityPolicy {
        network_allow,
        filesystem_scope,
        shell_allow,
        spawn_allow,
        shell_strip_env,
        shell_baseline_env,
        env_allow,
        limits,
    }
}
