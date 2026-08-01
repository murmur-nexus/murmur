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
    network_policy::{HookCapabilityGrant, ToolCapabilityGrant},
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
    /// Per-artifact capability grant copied verbatim from this artifact's entry in the
    /// capsule operator's own manifest (`murmur_artifact::RuntimeArtifact::capabilities`).
    /// Consumed for `runtime: hook` (default-deny baseline, `None` = deny everything) and for
    /// `runtime: tool`/`runtime: driver` (ceiling baseline, `None` = the capsule-wide policy
    /// unchanged); the manifest parser rejects the key on `runtime: skill`. Never sourced
    /// from the artifact's own bundled `murmur.yaml`, so an artifact cannot self-grant.
    pub capabilities: Option<murmur_artifact::Capabilities>,
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
    /// From `capabilities.network.unix_sockets`. `false` by default, which makes the shell
    /// subprocess tree's `socket(AF_UNIX, ...)` calls fail with `EACCES` at the kernel level on
    /// both Linux tiers — `network_allow` above governs IP destinations only and would otherwise
    /// leave a local daemon socket (`/var/run/docker.sock` — host root) completely unmediated.
    /// `true` simply omits that seccomp rule; it is capsule-wide, not per-socket-path. See
    /// `sandbox::denied_socket_domains`.
    pub unix_sockets_allowed: bool,
    pub filesystem_scope: Option<String>,
    pub shell_allow: Vec<String>,
    pub spawn_allow: Vec<String>,
    pub shell_strip_env: Vec<String>,
    pub shell_baseline_env: Vec<String>,
    /// Typed interpreter-runtime grants from `capabilities.shell.interpreter_runtime`: each
    /// names an already-allowlisted binary and the exact host directories outside the workdir
    /// its import machinery needs, with a per-directory `list_dir` enumerability flag. Empty
    /// unless the manifest declares them. Consumed on `KernelFull` to add narrow Landlock
    /// grants (see `sandbox::resolve_interpreter_runtime_grants`) and to fire `W-SEC-009`.
    pub shell_interpreter_runtime: Vec<murmur_artifact::InterpreterRuntimeGrant>,
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
    /// OS-level bounds applied to every *native subprocess* the session spawns — rlimits, the
    /// Linux cgroup v2 scope, and the workdir-size ceiling. Resolved on exactly the same terms
    /// as `limits` above (each omitted `capabilities.resources` field replaced by its default,
    /// no "unset" state), but a completely different subject: `limits` bounds a WASM guest
    /// inside its wasmtime store, this bounds the processes the host forks. See
    /// [`crate::resources`].
    pub resources: crate::resources::HostResourceLimits,
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
    /// Behavioral contract (binding/execution_mode/commit_policy) from the hook artifact's
    /// *own* bundled manifest. Carries no capability information — see `grant`.
    pub config: HookConfig,
    /// What this hook is allowed to do, lowered and validated at staging time from the
    /// operator's own manifest entry. Applied identically by all three hook instantiation
    /// paths in `hooks.rs`; `HookCapabilityGrant::default()` is full default-deny.
    pub grant: HookCapabilityGrant,
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
    /// Minimum containment class this session must achieve, already combined across every
    /// source that asked for one (manifest / workspace config / `--containment`) by taking the
    /// strongest. Required rather than optional: "nobody declared anything" is
    /// [`ContainmentClass::Advisory`], an explicit floor every host clears, not an absence.
    /// `stage_session` refuses to stage when the host cannot meet it.
    pub declared_containment_floor: murmur_artifact::ContainmentClass,
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
    /// Per-artifact narrowing for the tool/driver dispatch path, keyed by artifact name and
    /// lowered at staging time from the operator's own manifest entries. Holds an entry only
    /// for artifacts that actually declared a `capabilities:` block — a name absent from this
    /// map (the overwhelmingly common case) runs on the full capsule ceiling, exactly as
    /// before per-artifact narrowing existed.
    pub(crate) artifact_grants: HashMap<String, ToolCapabilityGrant>,
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
    /// Floor this session was staged against (copied from
    /// StageRequest::declared_containment_floor).
    pub(crate) declared_containment_floor: murmur_artifact::ContainmentClass,
    /// What the host actually provided, derived at staging time from the probed
    /// `EnforcementTier` alone. Recorded in `trace.jsonl`'s `session_start`. Never sourced from
    /// the manifest — see `containment::achieved_class_for_tier`.
    pub(crate) achieved_containment: murmur_artifact::ContainmentClass,
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
    // Absent `capabilities.network` block, or absent key within it, both mean denied.
    let unix_sockets_allowed = caps
        .and_then(|c| c.network.as_ref())
        .is_some_and(|network| network.unix_sockets);

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
    let shell_interpreter_runtime = caps
        .and_then(|c| c.shell.as_ref())
        .map(|shell| shell.interpreter_runtime.clone())
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

    let resources = crate::resources::resolve(caps.and_then(|c| c.resources.as_ref()));

    CapabilityPolicy {
        network_allow,
        unix_sockets_allowed,
        filesystem_scope,
        shell_allow,
        spawn_allow,
        shell_strip_env,
        shell_baseline_env,
        shell_interpreter_runtime,
        env_allow,
        limits,
        resources,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_for(manifest_yaml: &str) -> CapabilityPolicy {
        let manifest = murmur_artifact::RuntimeManifest::from_yaml_str(manifest_yaml)
            .expect("manifest fixture must parse");
        capability_policy_from_runtime_manifest(&manifest)
    }

    /// A manifest with no `capabilities:` block at all denies unix sockets — the field must not
    /// pick up any implicit widening from the "nothing declared" path.
    #[test]
    fn unix_sockets_denied_when_capabilities_block_is_absent() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
"#,
        );

        assert!(!policy.unix_sockets_allowed);
    }

    /// A declared `network:` block that never mentions the key still denies.
    #[test]
    fn unix_sockets_denied_when_network_block_omits_the_key() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
        );

        assert!(!policy.unix_sockets_allowed);
        assert_eq!(
            policy.network_allow,
            vec!["https://api.anthropic.com".to_string()]
        );
    }

    #[test]
    fn unix_sockets_denied_when_declared_false() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  network:
    unix_sockets: false
"#,
        );

        assert!(!policy.unix_sockets_allowed);
    }

    #[test]
    fn unix_sockets_allowed_when_declared_true() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  network:
    unix_sockets: true
"#,
        );

        assert!(policy.unix_sockets_allowed);
    }

    /// The opt-in is independent of the IP allowlist in both directions: a non-empty `allow`
    /// does not imply unix sockets, and `unix_sockets: true` does not imply any IP destination.
    #[test]
    fn unix_sockets_opt_in_is_independent_of_the_ip_allowlist() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  network:
    unix_sockets: true
    allow:
      - https://api.anthropic.com
"#,
        );

        assert!(policy.unix_sockets_allowed);
        assert_eq!(
            policy.network_allow,
            vec!["https://api.anthropic.com".to_string()]
        );
    }

    /// `CapabilityPolicy::default()` is what the sandbox tests and every `..Default::default()`
    /// call site start from; it must deny, not inherit whatever a future field ordering implies.
    #[test]
    fn default_capability_policy_denies_unix_sockets() {
        assert!(!CapabilityPolicy::default().unix_sockets_allowed);
    }
}
