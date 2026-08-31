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
    spawn_credential::SpawnCredential,
};

pub(crate) struct DispatchOutcome {
    pub result: ToolResult,
    pub shell: Option<ShellDispatchInfo>,
    /// Set instead of [`Self::shell`] when the command outran its grace period and was demoted
    /// to the background. The two are never both `Some`: a demoted command has no exit code yet,
    /// which is exactly why `HookEvent::Shell` does not fire for one.
    pub detached: Option<crate::detached::DetachedDispatchInfo>,
    /// True when the dispatch was served by reading a skill.md file rather than running
    /// a binary or WASM component. Used by the agent loop to write a `skill_call` trace
    /// event instead of a `tool_call` event.
    pub is_skill: bool,
    /// Set when this dispatch failed in a way that ends the *session* rather than just this
    /// tool call — today only a `sealed` composed-root construction failure
    /// ([`crate::shell::ShellExecError::session_fatal`]). `result` still describes the failed
    /// call so the trace records what was attempted; the agent turn loop then returns this
    /// error instead of feeding the failure back to the model for another turn.
    ///
    /// A dispatch that merely failed leaves this `None`: a capsule reacting to a broken tool
    /// call is ordinary, whereas a capsule continuing to run after its declared containment
    /// boundary stopped being establishable is the silent-degradation failure mode the whole
    /// containment-class mechanism exists to prevent.
    pub fatal: Option<crate::errors::RuntimeError>,
}

impl DispatchOutcome {
    pub(crate) fn tool(result: ToolResult) -> Self {
        Self {
            result,
            shell: None,
            detached: None,
            is_skill: false,
            fatal: None,
        }
    }

    pub(crate) fn skill(result: ToolResult) -> Self {
        Self {
            result,
            shell: None,
            detached: None,
            is_skill: true,
            fatal: None,
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
    /// What the runtime does when this hook's async job queue is full, copied verbatim from
    /// `on_overflow:` on this artifact's entry in the capsule operator's own manifest — the
    /// same operator-only sourcing rule `capabilities` above follows. Consumed for
    /// `runtime: hook` and inert for every other role (the manifest parser rejects the key
    /// there); inert too for a hook that turns out to be `execution_mode: blocking`, which is
    /// never queued.
    pub on_overflow: murmur_artifact::HookOverflowPolicy,
    /// This artifact's operator-declared `config:` block, copied verbatim from its entry in the
    /// capsule operator's own manifest (`murmur_artifact::RuntimeArtifact::config`) — the same
    /// operator-only sourcing rule `capabilities` above follows, so an artifact pulled from a
    /// registry cannot configure itself.
    ///
    /// Lowered to compact JSON at staging by [`crate::artifact_config::lower_artifact_config`] and
    /// delivered to this artifact alone as [`crate::artifact_config::ARTIFACT_CONFIG_ENV`]. `None`
    /// means the variable is absent from the guest environment. Consumed for `runtime: hook`,
    /// `runtime: tool` and `runtime: driver`; the manifest parser rejects the key on
    /// `runtime: skill`, and a native tool warns `W-SEC-015` because it reads no per-artifact
    /// environment at all.
    pub config: Option<serde_yaml::Value>,
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
    /// From `capabilities.peer_fetch.allow`: the peers this capsule may redeem a peer-file handle
    /// against. Empty by default, which is deny — and which is also why no `fetch-peer-file` tool
    /// manifest is written and the capsule's model never sees the tool exists.
    ///
    /// A separate list from [`Self::network_allow`], enforced with the same rule matcher but
    /// never merged into it. Ingesting a peer's bytes lands a file in this capsule's own workdir,
    /// so it is a prompt-injection surface and deserves its own operator control; a destination
    /// in `network.allow` is not redeemable unless it also appears here, and declaring it here
    /// does not widen `network.allow`.
    pub peer_fetch_allow: Vec<String>,
    /// From `capabilities.network.unix_sockets`. `false` by default, which makes the shell
    /// subprocess tree's `socket(AF_UNIX, ...)` calls fail with `EACCES` at the kernel level on
    /// both Linux tiers — `network_allow` above governs IP destinations only and would otherwise
    /// leave a local daemon socket (`/var/run/docker.sock` — host root) completely unmediated.
    /// `true` simply omits that seccomp rule; it is capsule-wide, not per-socket-path. See
    /// `sandbox::denied_socket_domains`.
    pub unix_sockets_allowed: bool,
    pub filesystem_scope: Option<String>,
    /// From `capabilities.filesystem.workdir_exec`. `false` by default, which withholds the
    /// Landlock `Execute` right from the workdir's own `PathBeneath` rule — see
    /// `sandbox::linux_enforce::workdir_access_rights`. With it withheld the kernel refuses to exec
    /// anything under the workdir on the resolved path itself, so `shell_allow` is enforced
    /// completely: a binary copied or compiled into the workdir cannot run under *any* name.
    ///
    /// `true` gives the right back for compile-and-run workflows and accepts, explicitly, that
    /// `shell_allow` is then unenforceable for anything inside the workdir. That is why this field
    /// is the one *grant* in this struct that also lowers the session's achieved containment class
    /// (see `crate::containment::achieved_containment_class`) — nothing else here can.
    pub workdir_exec_allowed: bool,
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
    /// Typed staged-runtime grants from `capabilities.shell.staged_runtime`: each names an
    /// already-allowlisted binary, the absolute host path of a pinned runtime tree, and the `pin`
    /// identifying that tree's build. Empty unless the manifest declares them.
    ///
    /// Consumed at staging to enforce that declaring one requires an effective `sealed` floor (see
    /// `crate::staged_runtime::check_staged_runtime_floor`) and to render the `staged runtime`
    /// section of `--explain-scope`. Per-binary mutually exclusive with
    /// [`Self::shell_interpreter_runtime`], which the manifest parser already guarantees.
    pub shell_staged_runtime: Vec<murmur_artifact::StagedRuntimeGrant>,
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
    /// The containment class this capsule asked for, from `capabilities.containment`.
    ///
    /// Unlike every other field here it is a *requirement*, not a grant — and unlike
    /// `StageRequest::declared_containment_floor` it is only the manifest's own vote, before the
    /// workspace config and `--containment` are folded in. `stage_session` overwrites it with the
    /// combined floor as soon as that is known, so anything reading it off a `StagedSession` sees
    /// the effective value; the manifest-only value exists so a `CapabilityPolicy` built straight
    /// from a manifest is still self-consistent.
    ///
    /// Read by `sandbox::ShellEnforcement::resolve` to decide whether a sealed-capable host
    /// actually installs a composed root for this session — a capsule that declared `scoped` must
    /// keep getting `scoped`'s mechanism. See `sandbox::applied_tier`.
    pub containment_floor: murmur_artifact::ContainmentClass,
    /// Whether the capsule's own top-level `capabilities.state` block was declared.
    ///
    /// Not a grant, and the only field here that is not: a durable state store is granted per
    /// *artifact*, so a capsule-wide declaration reaches nothing at all. It is recorded so
    /// `stage_session` can say that once (`W-SEC-014`) instead of leaving an operator to discover
    /// an empty directory, and it is read by nothing else.
    pub state_declared: bool,
    /// Recorded for the same reason [`Self::state_declared`] is, and applied just as little: the
    /// `murmur:conversation/read` grant is per artifact, so a capsule-wide declaration reaches
    /// nothing and `stage_session` says so once (`W-SEC-016`).
    pub conversation_declared: bool,
    /// `capabilities.limits.deadline_seconds` exactly as the manifest declared it — `None`
    /// when it declared nothing. Retained undefaulted alongside the fully-resolved `limits`
    /// above purely so hook calls can apply their own, lower default without mistaking an
    /// explicit `600` for silence. See [`Self::hook_limits`].
    pub declared_deadline_seconds: Option<u64>,
}

impl CapabilityPolicy {
    /// Execution limits for hook lifecycle calls — [`Self::limits`] with the hook-specific
    /// deadline default substituted when the manifest declared none.
    ///
    /// A hook call is one bounded piece of work, so it does not inherit the capsule-wide
    /// ten-minute budget that exists for a capsule's whole `run`: a wedged hook would
    /// otherwise stall the session for most of that, every event. An explicitly declared
    /// `deadline_seconds` still wins, for hooks as for every other guest.
    #[must_use]
    pub fn hook_limits(&self) -> ExecutionLimits {
        self.limits.for_hook_calls(self.declared_deadline_seconds)
    }
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
    /// What to do when this hook's async job queue is full — from the operator's manifest
    /// entry (via [`ArtifactRequest::on_overflow`]), on the same terms as `grant` and never
    /// from `config`. Read only when `config.execution_mode` is `Async`; a blocking hook has
    /// no queue to overflow.
    pub on_overflow: murmur_artifact::HookOverflowPolicy,
}

/// How `mur run --resume` puts the loaded conversation in front of the model.
///
/// `Full` is often the cheaper of the two: a verbatim reload can land on the provider's own
/// prompt cache, while compaction changes the prefix from the first altered token, guarantees a
/// cache miss, and costs an inference call to produce the summary. `Compact` is the answer when
/// the conversation would not fit the context window at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResumeMode {
    #[default]
    Full,
    Compact,
}

/// One operator act: continue *this* conversation, whatever the capsule's own
/// `lifecycle.conversation` policy says about carrying one between tasks.
///
/// Sugar over [`StageRequest::context_id`], never a second load path: the CLI resolves a session
/// address to the context id that session ran under and fills both this and `context_id`. What
/// this adds is the override — the record is loaded even under `lifecycle.conversation:
/// stateless` — and the provenance the trace records.
#[derive(Debug, Clone)]
pub struct ResumeRequest {
    /// The prior session's id, recorded verbatim as `session_start.resumed_from`.
    pub from_session: String,
    pub mode: ResumeMode,
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
    /// Whether `inference` above had its system-prompt declaration replaced by the caller
    /// (`mur run --system-prompt`) rather than read from the manifest. Purely a provenance
    /// signal for the trace: the override itself is already applied to `inference`, which by
    /// this point is indistinguishable from a manifest that declared the same prompt inline.
    /// `true` even when the override cleared the prompt to nothing.
    pub system_prompt_overridden: bool,
    pub context: Option<ContextConfig>,
    /// Context id every `task.md` task of this launch runs under, from `mur run --context`.
    /// `None` mints a fresh one per task. Validated by `stage_session` as one path segment: it is
    /// a directory name in the conversation record path.
    pub context_id: Option<String>,
    /// `mur run --resume <session>`, already resolved: `context_id` above holds the context that
    /// session ran under, and this carries the session's own id and the mode. `None` is every
    /// ordinary launch. `stage_session` refuses a resume whose context kept no record on disk,
    /// and a `--resume-mode compact` with no hook bound to `on-compaction`.
    pub resume: Option<ResumeRequest>,
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
    /// Minimum containment class this session must achieve, already combined across every
    /// source that asked for one (manifest / workspace config / `--containment`) by taking the
    /// strongest. Required rather than optional: "nobody declared anything" is
    /// [`ContainmentClass::Advisory`], an explicit floor every host clears, not an absence.
    /// `stage_session` refuses to stage when the host cannot meet it.
    pub declared_containment_floor: murmur_artifact::ContainmentClass,
    /// The manifest's top-level `exports:` block. `None` — and `Some` with no `files:` — means
    /// the resource plane is not declared and every request to it is denied.
    ///
    /// Carried beside `capability_policy` rather than inside it: an export is a disclosure the
    /// operator makes, not a capability the guest holds, and nothing derived from this field ever
    /// reaches the achieved containment class.
    pub exports: Option<murmur_artifact::Exports>,
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
    /// Copied from [`StageRequest::system_prompt_overridden`] — the only record left that the
    /// prompt in `inference` came from `--system-prompt` and not from the manifest. Passed to
    /// `TraceWriter::open`, which turns it into `session_start.system_prompt_source`.
    pub(crate) system_prompt_overridden: bool,
    pub(crate) context: Option<ContextConfig>,
    /// Copied from [`StageRequest::context_id`] and already validated. Read by `launch_session`,
    /// which uses it in place of a freshly minted id for every `task.md` task.
    pub(crate) context_id: Option<String>,
    /// Copied from [`StageRequest::resume`] and already checked. Read by `launch_session`, which
    /// turns it into `session_start.resumed_from` and the agent loop's record-load override.
    pub(crate) resume: Option<ResumeRequest>,
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
    /// How much of each turn's driver request this session's trace keeps (resolved from the
    /// manifest's `trace:` block, or [`murmur_artifact::TraceCapture::default`] when it has
    /// none).
    pub(crate) trace_capture: murmur_artifact::TraceCapture,
    /// What bounds this workdir's session directories, from the manifest's `trace.retain` block.
    /// `None` — the overwhelmingly common case — prunes nothing, ever.
    pub(crate) trace_retain: Option<murmur_artifact::TraceRetainConfig>,
    /// Address the HTTP server is bound to (copied from StageRequest::bind_addr).
    pub(crate) bind_addr: String,
    /// Internal port from the manifest (copied from StageRequest::internal_port).
    pub(crate) internal_port: Option<u16>,
    /// Floor this session was staged against (copied from
    /// StageRequest::declared_containment_floor).
    pub(crate) declared_containment_floor: murmur_artifact::ContainmentClass,
    /// The complete effective grant set this session was staged with — the same report
    /// `mur run --explain-scope --json` prints, built by `containment::scope_report_for_tier`
    /// from this session's own policy, declared floor and single host probe. Written verbatim to
    /// `trace.jsonl`'s `session_start` as `effective_grants`, and the source of the
    /// `containment_declared`/`containment_achieved`/`workdir_exec` fields alongside it, so the
    /// summary and the full report cannot disagree.
    ///
    /// Replaces a former separate `achieved_containment` field: what the host actually provided
    /// is `scope_report.achieved_containment`, and keeping a second copy beside it only created a
    /// way for the two to drift.
    pub(crate) scope_report: crate::containment::ScopeReport,
    /// The declared read-only file surface, with its root already checked against this session's
    /// accessible workdir by `stage_session` — a root that resolves outside it refuses the launch
    /// rather than being served. `None` means no resource plane.
    pub(crate) exports_files: Option<murmur_artifact::FileExport>,
    /// The declared peer-handoff surface, with its root already checked against this session's
    /// accessible workdir and its `max_ttl` already checked against `lifecycle.after_task` by
    /// `stage_session`. `None` means no peer plane: nothing mints, and `/resources/peer/` answers
    /// `no_peer_plane`.
    pub(crate) exports_peer_files: Option<murmur_artifact::PeerFilesExport>,
    /// Registry used to resolve this session's artifacts, retained so `manage.pull()` can
    /// resolve additional artifacts at runtime after staging has completed.
    pub(crate) registry: Arc<dyn Registry>,
    /// Keeps this session's epoch ticker running: without it no epoch deadline set on any
    /// store built from `engine` can ever fire. Held here (rather than detached) so ticking
    /// stops when the session's `StagedSession` drops. `launch_session` moves the other
    /// fields out one by one, which leaves this one in place until the session returns.
    pub(crate) _epoch_ticker: EpochTicker,
    /// The credential this session presents when it asks `mur-roost` to spawn a capsule.
    ///
    /// `None` for every session that cannot delegate, which is nearly all of them. Set by the
    /// caller between `stage_session` and `launch_session` rather than carried in
    /// [`StageRequest`]: the credential names a session id, and `stage_session` is what mints one,
    /// so it cannot exist before staging returns.
    pub(crate) spawn_credential: Option<SpawnCredential>,
}

impl StagedSession {
    /// Hands this session the credential it will present to the spawning daemon.
    pub fn set_spawn_credential(&mut self, credential: SpawnCredential) {
        self.spawn_credential = Some(credential);
    }

    /// The credential this session presents, if it was granted one.
    pub fn spawn_credential(&self) -> Option<&SpawnCredential> {
        self.spawn_credential.as_ref()
    }
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
    // Absent `capabilities.peer_fetch` block means denied, on the same terms — and separately
    // from `network.allow`, which this deliberately does not read.
    let peer_fetch_allow = caps
        .and_then(|c| c.peer_fetch.as_ref())
        .map(|peer_fetch| peer_fetch.allow.clone())
        .unwrap_or_default();

    // Absent `capabilities.network` block, or absent key within it, both mean denied.
    let unix_sockets_allowed = caps
        .and_then(|c| c.network.as_ref())
        .is_some_and(|network| network.unix_sockets);

    // Recorded, never applied: see `CapabilityPolicy::state_declared`.
    let state_declared = caps.is_some_and(|c| c.state.is_some());
    let conversation_declared = caps.is_some_and(|c| c.conversation.is_some());

    let filesystem_scope = caps
        .and_then(|c| c.filesystem.as_ref())
        .and_then(|filesystem| filesystem.scope.clone());
    // Absent `capabilities.filesystem` block, or absent key within it, both mean denied — the same
    // shape as `unix_sockets_allowed` above, for the same reason: the widening has to be declared.
    let workdir_exec_allowed = caps
        .and_then(|c| c.filesystem.as_ref())
        .is_some_and(|filesystem| filesystem.workdir_exec);

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
    let shell_staged_runtime = caps
        .and_then(|c| c.shell.as_ref())
        .map(|shell| shell.staged_runtime.clone())
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
    let declared_deadline_seconds = caps
        .and_then(|c| c.limits.as_ref())
        .and_then(|l| l.deadline_seconds);

    let resources = crate::resources::resolve(caps.and_then(|c| c.resources.as_ref()));

    CapabilityPolicy {
        network_allow,
        peer_fetch_allow,
        unix_sockets_allowed,
        filesystem_scope,
        workdir_exec_allowed,
        shell_allow,
        spawn_allow,
        shell_strip_env,
        shell_baseline_env,
        shell_interpreter_runtime,
        shell_staged_runtime,
        env_allow,
        limits,
        resources,
        containment_floor: caps.and_then(|c| c.containment).unwrap_or_default(),
        state_declared,
        conversation_declared,
        declared_deadline_seconds,
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

    /// A manifest that sets no deadline gives hook calls the hook-specific default while the
    /// capsule's own guests keep the capsule-wide one — one manifest, two budgets.
    #[test]
    fn a_silent_manifest_gives_hook_calls_the_lower_deadline() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
"#,
        );

        assert_eq!(
            policy.limits.deadline_seconds,
            crate::limits::DEFAULT_DEADLINE_SECONDS
        );
        assert_eq!(
            policy.hook_limits().deadline_seconds,
            crate::limits::HOOK_DEFAULT_DEADLINE_SECONDS
        );
        // The default policy a hand-built fixture gets must agree with the silent-manifest one.
        assert_eq!(
            CapabilityPolicy::default().hook_limits().deadline_seconds,
            crate::limits::HOOK_DEFAULT_DEADLINE_SECONDS
        );
    }

    /// An explicit `deadline_seconds` applies to every guest, hooks included — the hook
    /// default only fills a silence.
    #[test]
    fn an_explicit_deadline_applies_to_hook_calls_too() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  limits:
    deadline_seconds: 120
"#,
        );

        assert_eq!(policy.declared_deadline_seconds, Some(120));
        assert_eq!(policy.limits.deadline_seconds, 120);
        assert_eq!(policy.hook_limits().deadline_seconds, 120);
    }

    /// `capabilities.shell.staged_runtime` lowers onto the policy verbatim — binary, source path
    /// and pin all survive, since the pin is what a human later compares across two hosts.
    #[test]
    fn staged_runtime_grants_lower_onto_the_policy() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  containment: sealed
  shell:
    allow:
      - python3
    staged_runtime:
      - binary: python3
        source_path: /opt/testbed/conda/envs/django__django
        pin: conda-4.10.3/python-3.9.19
"#,
        );

        assert_eq!(
            policy.shell_staged_runtime,
            vec![murmur_artifact::StagedRuntimeGrant {
                binary: "python3".to_string(),
                source_path: "/opt/testbed/conda/envs/django__django".to_string(),
                pin: "conda-4.10.3/python-3.9.19".to_string(),
            }]
        );
        // The two mechanisms stay separate all the way down: lowering one must not populate the
        // other.
        assert!(policy.shell_interpreter_runtime.is_empty());
    }

    /// A shell block that declares no staged runtime lowers to an empty list, not to a default
    /// grant of any kind.
    #[test]
    fn absent_staged_runtime_lowers_to_empty() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  shell:
    allow:
      - bash
"#,
        );

        assert!(policy.shell_staged_runtime.is_empty());
    }

    /// The default every existing manifest gets. Three separate "nothing was declared" shapes —
    /// no `capabilities:` at all, no `filesystem:` within it, and a `filesystem:` that only sets
    /// `scope` — must all lower to denied, because each one is a real manifest in the wild and any
    /// of them silently keeping workdir `Execute` would reopen the rename-to-an-allowed-basename
    /// bypass this default exists to close.
    #[test]
    fn workdir_exec_denied_for_every_shape_of_undeclared() {
        for manifest_yaml in [
            r#"
name: cap
version: 0.1.0
artifacts: []
"#,
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  filesystem:
    scope: workdir
"#,
        ] {
            let policy = policy_for(manifest_yaml);
            assert!(
                !policy.workdir_exec_allowed,
                "an undeclared workdir_exec must lower to denied: {manifest_yaml}"
            );
        }
    }

    #[test]
    fn workdir_exec_denied_when_declared_false() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  filesystem:
    workdir_exec: false
"#,
        );

        assert!(!policy.workdir_exec_allowed);
    }

    #[test]
    fn workdir_exec_allowed_when_declared_true() {
        let policy = policy_for(
            r#"
name: cap
version: 0.1.0
artifacts: []
capabilities:
  filesystem:
    scope: workdir
    workdir_exec: true
"#,
        );

        assert!(policy.workdir_exec_allowed);
        // The two filesystem keys stay independent: lowering one must not disturb the other.
        assert_eq!(policy.filesystem_scope.as_deref(), Some("workdir"));
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
